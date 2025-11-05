//!Implementation of [`TaskManager`]
use super::processor::current_task;
use super::TaskControlBlock;
use crate::mm::MemorySet;
use crate::sync::UPSafeCell;
use alloc::sync::Arc;
use alloc::vec::Vec;
use lazy_static::*;
///A array of `TaskControlBlock` that is thread-safe
pub struct TaskManager {
    ready_queue: Vec<Arc<TaskControlBlock>>,
}

/// Stride-based scheduler.
impl TaskManager {
    ///Creat an empty TaskManager
    pub fn new() -> Self {
        Self {
            ready_queue: Vec::new(),
        }
    }
    /// Add process back to ready queue
    pub fn add(&mut self, task: Arc<TaskControlBlock>) {
        self.ready_queue.push(task);
    }
    /// Take a process out of the ready queue using stride scheduling
    pub fn fetch(&mut self) -> Option<Arc<TaskControlBlock>> {
        if self.ready_queue.is_empty() {
            return None;
        }
        let mut best_idx = 0;
        let mut best_stride = self.ready_queue[0].stride_value();
        for (idx, task) in self.ready_queue.iter().enumerate().skip(1) {
            let stride = task.stride_value();
            if stride < best_stride
                || (stride == best_stride && task.getpid() < self.ready_queue[best_idx].getpid())
            {
                best_idx = idx;
                best_stride = stride;
            }
        }
        Some(self.ready_queue.remove(best_idx))
    }
}

lazy_static! {
    /// TASK_MANAGER instance through lazy_static!
    pub static ref TASK_MANAGER: UPSafeCell<TaskManager> =
        unsafe { UPSafeCell::new(TaskManager::new()) };
}

/// Add process to ready queue
pub fn add_task(task: Arc<TaskControlBlock>) {
    //trace!("kernel: TaskManager::add_task");
    TASK_MANAGER.exclusive_access().add(task);
}

/// Take a process out of the ready queue
pub fn fetch_task() -> Option<Arc<TaskControlBlock>> {
    //trace!("kernel: TaskManager::fetch_task");
    TASK_MANAGER.exclusive_access().fetch()
}
/// Access the memory set of the current task
pub fn with_current_memory_set<F, R>(f: F) -> R
where
    F: FnOnce(&mut MemorySet) -> R,
{
    let task = current_task().expect("no current task available");
    let mut inner = task.inner_exclusive_access();
    f(&mut inner.memory_set)
}
