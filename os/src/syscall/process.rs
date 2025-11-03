//! Process management syscalls
use crate::config::PAGE_SIZE;
use crate::mm::{translated_byte_buffer, MapPermission, PTEFlags, PageTable, VirtAddr};
use crate::task::{
    change_program_brk, current_user_token, exit_current_and_run_next,
    suspend_current_and_run_next, syscall_count, with_current_memory_set,
};
use crate::timer::get_time_us;

#[repr(C)]
#[derive(Debug)]
pub struct TimeVal {
    pub sec: usize,
    pub usec: usize,
}

/// task exits and submit an exit code
pub fn sys_exit(_exit_code: i32) -> ! {
    trace!("kernel: sys_exit");
    exit_current_and_run_next();
    panic!("Unreachable in sys_exit!");
}

/// current task gives up resources for other tasks
pub fn sys_yield() -> isize {
    trace!("kernel: sys_yield");
    suspend_current_and_run_next();
    0
}

/// YOUR JOB: get time with second and microsecond
/// HINT: You might reimplement it with virtual memory management.
/// HINT: What if [`TimeVal`] is splitted by two pages ?
pub fn sys_get_time(_ts: *mut TimeVal, _tz: usize) -> isize {
    trace!("kernel: sys_get_time");
    if _ts.is_null() {
        return -1;
    }

    let now_us = get_time_us();
    let time_val = TimeVal {
        sec: now_us / 1_000_000,
        usec: now_us % 1_000_000,
    };
    let len = core::mem::size_of::<TimeVal>();
    let src =
        unsafe { core::slice::from_raw_parts((&time_val as *const TimeVal) as *const u8, len) };
    let mut written = 0;
    // Copy into user buffer which may span multiple pages.
    for dst in translated_byte_buffer(current_user_token(), _ts as *const u8, len) {
        let end = written + dst.len();
        dst.copy_from_slice(&src[written..end]);
        written = end;
    }
    debug_assert_eq!(written, len);
    0
}

/// TODO: Finish sys_trace to pass testcases
/// HINT: You might reimplement it with virtual memory management.
pub fn sys_trace(trace_request: usize, id: usize, data: usize) -> isize {
    trace!("kernel: sys_trace");
    match trace_request {
        0 => user_read_u8(id).map(|value| value as isize).unwrap_or(-1),
        1 => {
            if user_write_u8(id, data as u8).is_some() {
                0
            } else {
                -1
            }
        }
        2 => syscall_count(id) as isize,
        _ => -1,
    }
}

// YOUR JOB: Implement mmap.
pub fn sys_mmap(start: usize, len: usize, prot: usize) -> isize {
    trace!("kernel: sys_mmap");
    if start % PAGE_SIZE != 0 {
        return -1;
    }
    let perm = match prot_to_permission(prot) {
        Some(perm) => perm,
        None => return -1,
    };
    let aligned_len = match align_len_up(len) {
        Some(aligned) => aligned,
        None => return -1,
    };
    if aligned_len == 0 {
        return 0;
    }
    let end = match start.checked_add(aligned_len) {
        Some(end) => end,
        None => return -1,
    };
    let result = with_current_memory_set(|memory_set| {
        memory_set.mmap(VirtAddr::from(start), VirtAddr::from(end), perm)
    });
    if result.is_ok() {
        0
    } else {
        -1
    }
}

// YOUR JOB: Implement munmap.
pub fn sys_munmap(start: usize, len: usize) -> isize {
    trace!("kernel: sys_munmap");
    if start % PAGE_SIZE != 0 {
        return -1;
    }
    if len == 0 {
        return 0;
    }
    if len % PAGE_SIZE != 0 {
        return -1;
    }
    let end = match start.checked_add(len) {
        Some(end) => end,
        None => return -1,
    };
    let result = with_current_memory_set(|memory_set| {
        memory_set.munmap(VirtAddr::from(start), VirtAddr::from(end))
    });
    if result.is_ok() {
        0
    } else {
        -1
    }
}
/// change data segment size
pub fn sys_sbrk(size: i32) -> isize {
    trace!("kernel: sys_sbrk");
    if let Some(old_brk) = change_program_brk(size) {
        old_brk as isize
    } else {
        -1
    }
}

fn with_user_byte<R>(addr: usize, need_write: bool, f: impl FnOnce(&mut u8) -> R) -> Option<R> {
    let token = current_user_token();
    let page_table = PageTable::from_token(token);
    let va = VirtAddr::from(addr);
    let vpn = va.floor();
    let pte = page_table.translate(vpn)?;
    let flags = pte.flags();
    if !flags.contains(PTEFlags::V) || !flags.contains(PTEFlags::U) {
        return None;
    }
    if need_write {
        if !flags.contains(PTEFlags::W) {
            return None;
        }
    } else if !flags.contains(PTEFlags::R) {
        return None;
    }
    let offset = va.page_offset();
    let bytes = pte.ppn().get_bytes_array();
    Some(f(&mut bytes[offset]))
}

fn user_read_u8(addr: usize) -> Option<u8> {
    with_user_byte(addr, false, |byte| *byte)
}

fn user_write_u8(addr: usize, value: u8) -> Option<()> {
    with_user_byte(addr, true, |byte| {
        *byte = value;
    })
}

fn prot_to_permission(prot: usize) -> Option<MapPermission> {
    if prot & !0x7 != 0 {
        return None;
    }
    if prot & 0x7 == 0 {
        return None;
    }
    let mut perm = MapPermission::U;
    if prot & 0x1 != 0 {
        perm |= MapPermission::R;
    }
    if prot & 0x2 != 0 {
        perm |= MapPermission::W;
    }
    if prot & 0x4 != 0 {
        perm |= MapPermission::X;
    }
    Some(perm)
}

fn align_len_up(len: usize) -> Option<usize> {
    if len == 0 {
        return Some(0);
    }
    let pages = ((len - 1) / PAGE_SIZE).checked_add(1)?;
    pages.checked_mul(PAGE_SIZE)
}
