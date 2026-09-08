/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */
//! `semaphore.h`

use crate::dyld::{export_c_func, FunctionExports};
use crate::libc::errno::set_errno;
use crate::libc::posix_io::stat::mode_t;
use crate::libc::posix_io::{O_CREAT, O_EXCL};
use crate::mem::{ConstPtr, MutPtr, SafeRead};
use crate::{Environment, ThreadId};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use super::errno::EINVAL;

// Mach kernel return codes (from <mach/kern_return.h>). Only the ones
// relevant to semaphore_timedwait are needed here.
pub type kern_return_t = i32;
pub const KERN_SUCCESS: kern_return_t = 0;
pub const KERN_OPERATION_TIMED_OUT: kern_return_t = 49;
pub const KERN_INVALID_ARGUMENT: kern_return_t = 4;

/// `mach_timespec_t` from `<mach/mach_time.h>`. Distinct from POSIX
/// `struct timespec`: both fields are unsigned 32-bit, and the field order
/// is the same (seconds, then nanoseconds).
#[allow(non_camel_case_types)]
#[derive(Copy, Clone, Debug, Default)]
#[repr(C, packed)]
pub struct mach_timespec_t {
    pub tv_sec: u32,
    pub tv_nsec: u32,
}
unsafe impl SafeRead for mach_timespec_t {}

// SEM_FAILED is defined as -1 while having a type of sem_t *
pub const SEM_FAILED: MutPtr<sem_t> = MutPtr::from_bits(u32::MAX);

#[derive(Default)]
pub struct State {
    named_semaphores: HashMap<String, Rc<RefCell<SemaphoreHostObject>>>,
    pub open_semaphores: HashMap<MutPtr<sem_t>, Rc<RefCell<SemaphoreHostObject>>>,
}
impl State {
    fn get(env: &Environment) -> &Self {
        &env.libc_state.semaphore
    }
    fn get_mut(env: &mut Environment) -> &mut Self {
        &mut env.libc_state.semaphore
    }
}

#[allow(non_camel_case_types)]
pub type sem_t = i32;

pub struct SemaphoreHostObject {
    pub value: i32,
    pub waiting: HashSet<ThreadId>,
    guest_sem: Option<MutPtr<sem_t>>,
    named: bool,
}

pub fn sem_init(env: &mut Environment, sem: MutPtr<sem_t>, pshared: i32, value: u32) -> i32 {
    // TODO: handle errno properly
    set_errno(env, 0);

    assert!(pshared == 0);

    let state = State::get_mut(env);
    if state.open_semaphores.contains_key(&sem) {
        return 0;
    }
    let host_sem_rc = Rc::new(RefCell::new(SemaphoreHostObject {
        value: value as i32,
        waiting: HashSet::new(),
        guest_sem: Some(sem),
        named: false,
    }));

    state.open_semaphores.insert(sem, host_sem_rc);
    0
}

pub fn sem_destroy(env: &mut Environment, sem: MutPtr<sem_t>) -> i32 {
    let state = State::get_mut(env);
    let sem = state.open_semaphores.remove(&sem);
    if let Some(sem) = sem {
        assert!(!sem.borrow().named);
        // Don't free, it's not our resposibility to.
        0
    } else {
        // No semaphores at that pointer.
        EINVAL
    }
}

pub fn sem_open(
    env: &mut Environment,
    name: ConstPtr<u8>,
    oflag: i32,
    _mode: mode_t,
    value: u32,
) -> MutPtr<sem_t> {
    // TODO: handle errno properly
    set_errno(env, 0);

    let sem_name = env.mem.cstr_at_utf8(name).unwrap();
    let sem_name_str = sem_name.to_string();
    let host_sem_rc =
        if let Some(existing_host_sem_rc) = State::get(env).named_semaphores.get(sem_name) {
            if (oflag & O_EXCL) == 0 {
                // TODO: set errno
                return SEM_FAILED;
            }
            let existing_host_sem = (*existing_host_sem_rc).borrow();
            if let Some(existing_sem) = existing_host_sem.guest_sem {
                return existing_sem;
            }
            existing_host_sem_rc.clone()
        } else {
            if (oflag & O_CREAT) == 0 {
                // TODO: set errno
                return SEM_FAILED;
            }
            let host_sem_rc = Rc::new(RefCell::new(SemaphoreHostObject {
                value: value as i32,
                waiting: HashSet::new(),
                guest_sem: None,
                named: true,
            }));
            State::get_mut(env)
                .named_semaphores
                .insert(sem_name_str, Rc::clone(&host_sem_rc));
            host_sem_rc
        };

    let sem = env.mem.alloc_and_write(0);
    (*host_sem_rc).borrow_mut().guest_sem = Some(sem);
    State::get_mut(env).open_semaphores.insert(sem, host_sem_rc);

    sem
}

pub fn sem_post(env: &mut Environment, sem: MutPtr<sem_t>) -> i32 {
    // TODO: handle errno properly
    set_errno(env, 0);

    env.sem_increment(sem);
    0 // success
}

pub fn sem_wait(env: &mut Environment, sem: MutPtr<sem_t>) -> i32 {
    // TODO: handle errno properly
    set_errno(env, 0);

    env.sem_decrement(sem, true);
    0 // success
}

fn sem_trywait(env: &mut Environment, sem: MutPtr<sem_t>) -> i32 {
    // TODO: handle errno properly
    set_errno(env, 0);

    if env.sem_decrement(sem, false) {
        0 // success
    } else {
        -1
    }
}

/// `semaphore_timedwait` from `<mach/semaphore.h>`.
///
/// This is a Mach kernel primitive, distinct from POSIX `sem_wait`/
/// `sem_timedwait`: it takes a `mach_timespec_t` (absolute deadline, wall
/// clock) rather than a relative `struct timespec`, and it returns a
/// `kern_return_t` (`KERN_SUCCESS` / `KERN_OPERATION_TIMED_OUT` /
/// `KERN_INVALID_ARGUMENT`) rather than the POSIX `0`/`-1` + `errno`
/// convention.
///
/// touchHLE does not currently distinguish Mach semaphore ports from POSIX
/// `sem_t*` handles: both are represented as a `MutPtr<sem_t>` key into
/// `open_semaphores`, so the same decrement/blocking machinery is reused
/// here.
pub fn semaphore_timedwait(
    env: &mut Environment,
    sem: MutPtr<sem_t>,
    wait_time: mach_timespec_t,
) -> kern_return_t {
    if !env
        .libc_state
        .semaphore
        .open_semaphores
        .contains_key(&sem)
    {
        return KERN_INVALID_ARGUMENT;
    }

    // Fast path: already available, no need to touch the scheduler's
    // timeout bookkeeping at all.
    if env.sem_decrement(sem, false) {
        return KERN_SUCCESS;
    }

    // `wait_time` is a wall-clock absolute deadline (seconds/nanoseconds
    // since the UNIX epoch), matching how Apple's semaphore_timedwait is
    // documented: the caller typically computes it from the current time
    // plus some relative offset before calling in. touchHLE's scheduler
    // works in terms of [Instant], so convert by measuring the deadline's
    // offset from "now" in both clocks.
    let deadline_since_epoch = Duration::new(wait_time.tv_sec as u64, wait_time.tv_nsec);
    let now_since_epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let deadline_instant = if deadline_since_epoch > now_since_epoch {
        Instant::now() + (deadline_since_epoch - now_since_epoch)
    } else {
        // Deadline is already in the past (or equal to now): expire
        // immediately rather than underflowing the subtraction.
        Instant::now()
    };

    if env.sem_decrement_timed(sem, deadline_instant) {
        KERN_SUCCESS
    } else {
        KERN_OPERATION_TIMED_OUT
    }
}

pub fn sem_close(env: &mut Environment, sem: MutPtr<sem_t>) -> i32 {
    // TODO: handle errno properly
    set_errno(env, 0);

    let host_sem_rc = env
        .libc_state
        .semaphore
        .open_semaphores
        .remove(&sem)
        .unwrap();
    let mut host_sem = (*host_sem_rc).borrow_mut();
    assert!(host_sem.named);
    env.mem.free(host_sem.guest_sem.unwrap().cast());
    host_sem.guest_sem = None;
    0 // success
}

pub fn sem_unlink(env: &mut Environment, name: ConstPtr<u8>) -> i32 {
    // TODO: handle errno properly
    set_errno(env, 0);

    let sem_name = env.mem.cstr_at_utf8(name).unwrap();
    env.libc_state.semaphore.named_semaphores.remove(sem_name);
    0 // success
}

/// Shortcut for host code to make an unnamed semaphore. Destroy with
/// host_destroy_semaphore, not sem_destroy
pub fn host_create_semaphore(env: &mut Environment, value: u32) -> MutPtr<sem_t> {
    let sem: MutPtr<sem_t> = env.mem.alloc_and_write(0);
    sem_init(env, sem, 0, value);
    sem
}
pub fn host_destroy_semaphore(env: &mut Environment, sem: MutPtr<sem_t>) {
    sem_destroy(env, sem);
    env.mem.free(sem.cast());
}

pub const FUNCTIONS: FunctionExports = &[
    export_c_func!(sem_init(_, _, _)),
    export_c_func!(sem_destroy(_)),
    export_c_func!(sem_open(_, _, _, _)),
    export_c_func!(sem_post(_)),
    export_c_func!(sem_wait(_)),
    export_c_func!(sem_trywait(_)),
    export_c_func!(sem_close(_)),
    export_c_func!(sem_unlink(_)),
    export_c_func!(semaphore_timedwait(_, _)),
];
