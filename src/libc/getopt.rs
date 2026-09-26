/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */
//! `getopt.h` / BSD `unistd.h` command-line option parsing:
//! `getopt()`, `getopt_long()`, and the `optarg`/`optind`/`opterr`/`optopt`
//! globals they communicate through.
//!
//! Apple references:
//! * `getopt(3)`:
//!   <https://developer.apple.com/library/archive/documentation/System/Conceptual/ManPages_iPhoneOS/man3/getopt.3.html>
//! * `getopt_long(3)`:
//!   <https://developer.apple.com/library/archive/documentation/System/Conceptual/ManPages_iPhoneOS/man3/getopt_long.3.html>
//!
//! iOS apps essentially never call these (there's no command line), but
//! some cross-platform engines (SDL-based games, ports of desktop tools,
//! Mono/.NET runtimes handling `Environment.GetCommandLineArgs()`-style
//! init code) reference `getopt_long` unconditionally during startup even
//! when it's never actually invoked with real arguments, so touchHLE's
//! dynamic linker needs a real symbol here rather than a return-0 stub.

use crate::dyld::{export_c_func, ConstantExports, FunctionExports, HostConstant};
use crate::mem::{ConstPtr, MutPtr, Ptr};
use crate::Environment;

#[derive(Default)]
pub struct State {
    /// Address of the guest `optarg` (`char *`) storage cell.
    optarg_ptr: Option<MutPtr<ConstPtr<u8>>>,
    /// Address of the guest `optind` (`int`) storage cell.
    optind_ptr: Option<MutPtr<i32>>,
    /// Address of the guest `opterr` (`int`) storage cell.
    opterr_ptr: Option<MutPtr<i32>>,
    /// Address of the guest `optopt` (`int`) storage cell.
    optopt_ptr: Option<MutPtr<i32>>,
    /// Index into the current `argv` element when parsing a bundle of
    /// short options like `-abc`, mirroring glibc/BSD internal state.
    next_char_idx: usize,
}

fn optarg_ptr(env: &mut Environment) -> crate::mem::ConstVoidPtr {
    let ptr: MutPtr<ConstPtr<u8>> = env.mem.alloc_and_write(Ptr::null());
    env.libc_state.getopt.optarg_ptr = Some(ptr);
    ptr.cast().cast_const()
}

fn optind_ptr(env: &mut Environment) -> crate::mem::ConstVoidPtr {
    // POSIX: optind starts at 1 (argv[0] is the program name and is
    // always skipped).
    let ptr: MutPtr<i32> = env.mem.alloc_and_write(1i32);
    env.libc_state.getopt.optind_ptr = Some(ptr);
    ptr.cast().cast_const()
}

fn opterr_ptr(env: &mut Environment) -> crate::mem::ConstVoidPtr {
    // POSIX: opterr defaults to nonzero (getopt prints its own error
    // messages) unless the app sets it to 0.
    let ptr: MutPtr<i32> = env.mem.alloc_and_write(1i32);
    env.libc_state.getopt.opterr_ptr = Some(ptr);
    ptr.cast().cast_const()
}

fn optopt_ptr(env: &mut Environment) -> crate::mem::ConstVoidPtr {
    let ptr: MutPtr<i32> = env.mem.alloc_and_write(0i32);
    env.libc_state.getopt.optopt_ptr = Some(ptr);
    ptr.cast().cast_const()
}

fn get_optind(env: &mut Environment) -> i32 {
    let ptr = env.libc_state.getopt.optind_ptr.unwrap_or_else(|| {
        let p: MutPtr<i32> = env.mem.alloc_and_write(1i32);
        env.libc_state.getopt.optind_ptr = Some(p);
        p
    });
    env.mem.read(ptr)
}
fn set_optind(env: &mut Environment, val: i32) {
    let ptr = env.libc_state.getopt.optind_ptr.unwrap_or_else(|| {
        let p: MutPtr<i32> = env.mem.alloc_and_write(1i32);
        env.libc_state.getopt.optind_ptr = Some(p);
        p
    });
    env.mem.write(ptr, val);
}
fn set_optarg(env: &mut Environment, val: ConstPtr<u8>) {
    let ptr = env.libc_state.getopt.optarg_ptr.unwrap_or_else(|| {
        let p: MutPtr<ConstPtr<u8>> = env.mem.alloc_and_write(Ptr::null());
        env.libc_state.getopt.optarg_ptr = Some(p);
        p
    });
    env.mem.write(ptr, val);
}
fn set_optopt(env: &mut Environment, val: i32) {
    let ptr = env.libc_state.getopt.optopt_ptr.unwrap_or_else(|| {
        let p: MutPtr<i32> = env.mem.alloc_and_write(0i32);
        env.libc_state.getopt.optopt_ptr = Some(p);
        p
    });
    env.mem.write(ptr, val);
}
fn get_opterr(env: &mut Environment) -> i32 {
    let ptr = env.libc_state.getopt.opterr_ptr.unwrap_or_else(|| {
        let p: MutPtr<i32> = env.mem.alloc_and_write(1i32);
        env.libc_state.getopt.opterr_ptr = Some(p);
        p
    });
    env.mem.read(ptr)
}

/// Read `argv[idx]` (a `char *const argv[]` guest array) as a UTF-8
/// `String`, or `None` past the end (`argc`) or on decode failure.
fn read_arg(env: &mut Environment, argv: MutPtr<MutPtr<u8>>, argc: i32, idx: i32) -> Option<String> {
    if idx < 0 || idx >= argc {
        return None;
    }
    let arg_ptr = env.mem.read(argv + idx as u32);
    if arg_ptr.is_null() {
        return None;
    }
    env.mem.cstr_at_utf8(arg_ptr.cast_const()).ok().map(|s| s.to_owned())
}

/// Shared short-option scanner used by both `getopt()` and the
/// short-option fallback path of `getopt_long()`.
///
/// `optstring` follows POSIX syntax: a letter with no suffix takes no
/// argument, a letter followed by `:` requires an argument (optionally
/// attached, e.g. `-oFILE`, or as the next `argv` element, e.g. `-o
/// FILE`), and a letter followed by `::` takes an optional attached
/// argument (a GNU extension some Mono/SDL code paths rely on).
fn do_getopt(
    env: &mut Environment,
    argc: i32,
    argv: MutPtr<MutPtr<u8>>,
    optstring: &str,
) -> i32 {
    let mut ind = get_optind(env);
    // Skip past any already-consumed non-option leading arguments the
    // first time we're called, mirroring glibc's lazy initialisation.
    loop {
        let Some(current) = read_arg(env, argv, argc, ind) else {
            set_optind(env, ind);
            return -1; // no more arguments
        };

        let chars: Vec<char> = current.chars().collect();
        let at_start_of_new_word = env.libc_state.getopt.next_char_idx == 0;

        if at_start_of_new_word {
            if current == "--" {
                set_optind(env, ind + 1);
                return -1;
            }
            if chars.len() < 2 || chars[0] != '-' {
                // Not an option (POSIX getopt(), unlike GNU, stops at the
                // first non-option argument since we don't implement
                // permutation of argv).
                set_optind(env, ind);
                return -1;
            }
            env.libc_state.getopt.next_char_idx = 1;
        }

        let char_idx = env.libc_state.getopt.next_char_idx;
        if char_idx >= chars.len() {
            // Consumed the whole word; move to the next argv element.
            env.libc_state.getopt.next_char_idx = 0;
            ind += 1;
            continue;
        }

        let opt = chars[char_idx];
        env.libc_state.getopt.next_char_idx += 1;

        // Find `opt` in optstring (skipping a possible leading ':' or
        // '+' which only affect error-reporting/permutation modes we
        // don't implement).
        let opt_bytes: Vec<char> = optstring.chars().collect();
        let Some(spec_idx) = opt_bytes.iter().position(|&c| c == opt && c != ':') else {
            set_optopt(env, opt as i32);
            if get_opterr(env) != 0 && !optstring.starts_with(':') {
                log!("getopt: invalid option -- '{}'", opt);
            }
            if env.libc_state.getopt.next_char_idx >= chars.len() {
                env.libc_state.getopt.next_char_idx = 0;
                ind += 1;
            }
            set_optind(env, ind);
            return b'?' as i32;
        };

        let takes_arg = opt_bytes.get(spec_idx + 1) == Some(&':');
        let optional_arg = takes_arg && opt_bytes.get(spec_idx + 2) == Some(&':');

        if !takes_arg {
            set_optarg(env, Ptr::null());
            if env.libc_state.getopt.next_char_idx >= chars.len() {
                env.libc_state.getopt.next_char_idx = 0;
                ind += 1;
            }
            set_optind(env, ind);
            return opt as i32;
        }

        // Argument attached to the same word, e.g. `-oFILE`.
        if env.libc_state.getopt.next_char_idx < chars.len() {
            let rest: String = chars[env.libc_state.getopt.next_char_idx..].iter().collect();
            let arg_ptr = env.mem.alloc_and_write_cstr(rest.as_bytes());
            set_optarg(env, arg_ptr.cast_const());
            env.libc_state.getopt.next_char_idx = 0;
            ind += 1;
            set_optind(env, ind);
            return opt as i32;
        }

        // No attached argument: consume the next argv element, unless
        // this option's argument is optional (GNU `::` extension), in
        // which case an unattached argument is *not* consumed.
        env.libc_state.getopt.next_char_idx = 0;
        if optional_arg {
            set_optarg(env, Ptr::null());
            ind += 1;
            set_optind(env, ind);
            return opt as i32;
        }

        if let Some(next) = read_arg(env, argv, argc, ind + 1) {
            let arg_ptr = env.mem.alloc_and_write_cstr(next.as_bytes());
            set_optarg(env, arg_ptr.cast_const());
            ind += 2;
            set_optind(env, ind);
            return opt as i32;
        } else {
            set_optopt(env, opt as i32);
            if get_opterr(env) != 0 && !optstring.starts_with(':') {
                log!("getopt: option requires an argument -- '{}'", opt);
            }
            ind += 1;
            set_optind(env, ind);
            return if optstring.starts_with(':') {
                b':' as i32
            } else {
                b'?' as i32
            };
        }
    }
}

/// `int getopt(int argc, char *const argv[], const char *optstring);`
fn getopt(
    env: &mut Environment,
    argc: i32,
    argv: MutPtr<MutPtr<u8>>,
    optstring: ConstPtr<u8>,
) -> i32 {
    let optstring = env.mem.cstr_at_utf8(optstring).unwrap_or("").to_owned();
    do_getopt(env, argc, argv, &optstring)
}

/// Mirrors `struct option` from `<getopt.h>`:
/// ```c
/// struct option {
///     const char *name;
///     int         has_arg; // no_argument=0, required_argument=1, optional_argument=2
///     int        *flag;
///     int         val;
/// };
/// ```
#[allow(dead_code)]
struct GuestOption {
    name: ConstPtr<u8>,
    has_arg: i32,
    flag: MutPtr<i32>,
    val: i32,
}

const NO_ARGUMENT: i32 = 0;
const REQUIRED_ARGUMENT: i32 = 1;
const OPTIONAL_ARGUMENT: i32 = 2;

/// Reads one `struct option` guest array element manually (touchHLE
/// doesn't derive `GuestType` for this file's types here, so we read
/// the four fields at their known offsets: three 4-byte fields plus a
/// pointer, all pointer/int-sized on the 32-bit ABI touchHLE targets).
fn read_option(env: &mut Environment, base: MutPtr<u8>, index: u32) -> GuestOption {
    let entry = base + index * 16; // sizeof(struct option) == 16 on ILP32
    let name: ConstPtr<u8> = env.mem.read(entry.cast());
    let has_arg: i32 = env.mem.read((entry + 4).cast());
    let flag: MutPtr<i32> = env.mem.read((entry + 8).cast());
    let val: i32 = env.mem.read((entry + 12).cast());
    GuestOption {
        name,
        has_arg,
        flag,
        val,
    }
}

/// Shared implementation for `getopt_long()` and `getopt_long_only()`.
fn do_getopt_long(
    env: &mut Environment,
    argc: i32,
    argv: MutPtr<MutPtr<u8>>,
    optstring: ConstPtr<u8>,
    longopts: MutPtr<u8>,
    longindex: MutPtr<i32>,
    long_only: bool,
) -> i32 {
    let ind = get_optind(env);
    let Some(current) = read_arg(env, argv, argc, ind) else {
        return -1;
    };

    let is_long = current.starts_with("--")
        || (long_only && current.starts_with('-') && current.len() > 1);

    if !current.starts_with('-') || current == "--" || !is_long {
        // Not a long option (or bare "--"/short option): defer to the
        // short-option scanner, which also handles end-of-options.
        let optstring_str = env.mem.cstr_at_utf8(optstring).unwrap_or("").to_owned();
        return do_getopt(env, argc, argv, &optstring_str);
    }

    let name_start = if current.starts_with("--") { 2 } else { 1 };
    let rest = &current[name_start..];
    let (name, attached_arg) = match rest.split_once('=') {
        Some((n, v)) => (n, Some(v.to_owned())),
        None => (rest, None),
    };

    if name.is_empty() {
        // Bare "--": end of options.
        set_optind(env, ind + 1);
        return -1;
    }

    // Walk the guest `longopts` array until a NULL-name sentinel,
    // looking for an exact or unambiguous-prefix match (GNU semantics).
    let mut found: Option<(u32, GuestOption)> = None;
    let mut ambiguous = false;
    let mut i = 0u32;
    loop {
        let opt = read_option(env, longopts, i);
        if opt.name.is_null() {
            break;
        }
        let opt_name = env.mem.cstr_at_utf8(opt.name).unwrap_or("").to_owned();
        if opt_name == name {
            found = Some((i, opt));
            ambiguous = false;
            break;
        }
        if opt_name.starts_with(name) {
            if found.is_some() {
                ambiguous = true;
            }
            found = Some((i, opt));
        }
        i += 1;
    }

    if ambiguous {
        log!("getopt_long: option '--{}' is ambiguous", name);
        set_optind(env, ind + 1);
        return b'?' as i32;
    }

    let Some((found_idx, opt)) = found else {
        if get_opterr(env) != 0 {
            log!("getopt_long: unrecognized option '--{}'", name);
        }
        set_optind(env, ind + 1);
        return b'?' as i32;
    };

    if !longindex.is_null() {
        env.mem.write(longindex, found_idx as i32);
    }

    let arg_value: Option<String> = match (opt.has_arg, &attached_arg) {
        (NO_ARGUMENT, Some(_)) => {
            log!(
                "getopt_long: option '--{}' doesn't allow an argument",
                name
            );
            set_optind(env, ind + 1);
            return b'?' as i32;
        }
        (NO_ARGUMENT, None) => None,
        (REQUIRED_ARGUMENT, Some(v)) => Some(v.clone()),
        (REQUIRED_ARGUMENT, None) => match read_arg(env, argv, argc, ind + 1) {
            Some(v) => {
                set_optind(env, ind + 2);
                if opt.flag.is_null() {
                    let arg_ptr = env.mem.alloc_and_write_cstr(v.as_bytes());
                    set_optarg(env, arg_ptr.cast_const());
                } else {
                    env.mem.write(opt.flag, opt.val);
                }
                let ret = if opt.flag.is_null() { opt.val } else { 0 };
                return ret;
            }
            None => {
                log!("getopt_long: option '--{}' requires an argument", name);
                set_optind(env, ind + 1);
                return b'?' as i32;
            }
        },
        (OPTIONAL_ARGUMENT, v) => v.clone(),
        _ => None,
    };

    if let Some(v) = &arg_value {
        let arg_ptr = env.mem.alloc_and_write_cstr(v.as_bytes());
        set_optarg(env, arg_ptr.cast_const());
    } else {
        set_optarg(env, Ptr::null());
    }

    set_optind(env, ind + 1);

    if !opt.flag.is_null() {
        env.mem.write(opt.flag, opt.val);
        0
    } else {
        opt.val
    }
}

/// `int getopt_long(int argc, char *const argv[], const char *optstring, const struct option *longopts, int *longindex);`
fn getopt_long(
    env: &mut Environment,
    argc: i32,
    argv: MutPtr<MutPtr<u8>>,
    optstring: ConstPtr<u8>,
    longopts: MutPtr<u8>,
    longindex: MutPtr<i32>,
) -> i32 {
    do_getopt_long(env, argc, argv, optstring, longopts, longindex, false)
}

/// `int getopt_long_only(int argc, char *const argv[], const char *optstring, const struct option *longopts, int *longindex);`
/// Same as `getopt_long()`, but a single leading `-` is also accepted
/// to introduce a long option (only falling back to short-option
/// parsing if no long-option name matches).
fn getopt_long_only(
    env: &mut Environment,
    argc: i32,
    argv: MutPtr<MutPtr<u8>>,
    optstring: ConstPtr<u8>,
    longopts: MutPtr<u8>,
    longindex: MutPtr<i32>,
) -> i32 {
    do_getopt_long(env, argc, argv, optstring, longopts, longindex, true)
}

pub const CONSTANTS: ConstantExports = &[
    ("_optarg", HostConstant::Custom(optarg_ptr)),
    ("_optind", HostConstant::Custom(optind_ptr)),
    ("_opterr", HostConstant::Custom(opterr_ptr)),
    ("_optopt", HostConstant::Custom(optopt_ptr)),
];

pub const FUNCTIONS: FunctionExports = &[
    export_c_func!(getopt(_, _, _)),
    export_c_func!(getopt_long(_, _, _, _, _)),
    export_c_func!(getopt_long_only(_, _, _, _, _)),
];
