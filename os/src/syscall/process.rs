//! Process management syscalls
use crate::{
    task::{exit_current_and_run_next, suspend_current_and_run_next, get_current_task},
    timer::get_time_us,
    syscall::{
        SYSCALL_EXIT, SYSCALL_GET_TIME, SYSCALL_TRACE, SYSCALL_WRITE, SYSCALL_YIELD,
    },
};

#[repr(C)]
#[derive(Debug)]
pub struct TimeVal {
    pub sec: usize,
    pub usec: usize,
}

/// task exits and submit an exit code
pub fn sys_exit(exit_code: i32) -> ! {
    trace!("[kernel] Application exited with code {}", exit_code);
    exit_current_and_run_next();
    panic!("Unreachable in sys_exit!");
}

/// current task gives up resources for other tasks
pub fn sys_yield() -> isize {
    trace!("kernel: sys_yield");
    suspend_current_and_run_next();
    0
}

/// get time with second and microsecond
pub fn sys_get_time(ts: *mut TimeVal, _tz: usize) -> isize {
    trace!("kernel: sys_get_time");
    let us = get_time_us();
    unsafe {
        *ts = TimeVal {
            sec: us / 1_000_000,
            usec: us % 1_000_000,
        };
    }
    0
}

// TODO: implement the syscall
pub fn sys_trace(trace_request: usize, id: usize, data: usize) -> isize {
    match trace_request {
        //将id视为*const u8，忽略data，返回id地址处的值。
        0 =>  {
            unsafe {
                let ptr = id as *const u8;
                let value = *ptr;
                value as isize
            }
        }
        1 => {
            unsafe {
                let ptr = id as *mut u8;
                *ptr = data as u8;
            }
            0
        }
        2 => {
            let class: usize = match id {
                SYSCALL_WRITE => {
                    0
                }
                SYSCALL_EXIT => {
                    1
                }
                SYSCALL_YIELD => {
                    2
                }
                SYSCALL_GET_TIME => {
                    3
                }
                SYSCALL_TRACE => {
                    4
                }
                _ => {
                    panic!("Invalid syscall id {}", id);
                }
            };
            get_current_task().syscall_count[class] as isize
        }
        _ => {
            -1
        }
    }
}
