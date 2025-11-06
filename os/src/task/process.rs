//! Implementation of  [`ProcessControlBlock`]

use super::id::RecycleAllocator;
use super::manager::insert_into_pid2process;
use super::TaskControlBlock;
use super::{add_task, SignalFlags};
use super::{pid_alloc, PidHandle};
use crate::fs::{File, Stdin, Stdout};
use crate::mm::{translated_refmut, MemorySet, KERNEL_SPACE};
use crate::sync::{Condvar, Mutex, Semaphore, UPSafeCell};
use crate::trap::{trap_handler, TrapContext};
use alloc::collections::{BTreeMap, BTreeSet, VecDeque};
use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec;
use alloc::vec::Vec;
use core::cell::RefMut;

#[derive(Default)]
pub struct DeadlockState {
    enabled: bool,
    mutex: MutexDeadlockState,
    semaphore: SemaphoreDeadlockState,
}
impl DeadlockState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    pub fn register_mutex(&mut self, mutex_id: usize) {
        self.mutex.register(mutex_id);
    }

    pub fn register_semaphore(&mut self, sem_id: usize, initial: usize) {
        self.semaphore.register(sem_id, initial);
    }

    pub fn before_mutex_lock(&mut self, tid: usize, mutex_id: usize) -> Result<(), ()> {
        self.mutex.before_lock(self.enabled, tid, mutex_id)
    }

    pub fn after_mutex_lock(&mut self, tid: usize, mutex_id: usize) {
        self.mutex.after_lock(tid, mutex_id);
    }

    pub fn mutex_unlock(&mut self, tid: usize, mutex_id: usize) {
        self.mutex.unlock(tid, mutex_id);
    }

    pub fn before_semaphore_down(&mut self, tid: usize, sem_id: usize) -> Result<bool, ()> {
        self.semaphore.before_down(self.enabled, tid, sem_id)
    }

    pub fn after_semaphore_down(&mut self, tid: usize, sem_id: usize, immediate: bool) {
        self.semaphore.after_down(tid, sem_id, immediate);
    }

    pub fn semaphore_up(&mut self, tid: usize, sem_id: usize) {
        self.semaphore.up(tid, sem_id);
    }

    pub fn cleanup_thread(&mut self, tid: usize) {
        self.mutex.cleanup_thread(tid);
        self.semaphore.cleanup_thread(tid);
    }
}

#[derive(Default)]
struct MutexDeadlockState {
    owners: Vec<Option<usize>>,
    waiting: BTreeMap<usize, usize>,
}

impl MutexDeadlockState {
    fn ensure(&mut self, mutex_id: usize) {
        while self.owners.len() <= mutex_id {
            self.owners.push(None);
        }
    }

    fn register(&mut self, mutex_id: usize) {
        self.ensure(mutex_id);
        if let Some(owner) = self.owners.get_mut(mutex_id) {
            *owner = None;
        }
        self.waiting.retain(|_, wait_mutex| *wait_mutex != mutex_id);
    }

    fn before_lock(&mut self, enabled: bool, tid: usize, mutex_id: usize) -> Result<(), ()> {
        self.ensure(mutex_id);
        self.waiting.remove(&tid);
        match self.owners[mutex_id] {
            None => Ok(()),
            Some(owner_tid) if owner_tid == tid => {
                if enabled {
                    Err(())
                } else {
                    self.waiting.insert(tid, mutex_id);
                    Ok(())
                }
            }
            Some(owner_tid) => {
                if enabled && self.detect_cycle(tid, owner_tid) {
                    Err(())
                } else {
                    self.waiting.insert(tid, mutex_id);
                    Ok(())
                }
            }
        }
    }

    fn detect_cycle(&self, start_tid: usize, owner_tid: usize) -> bool {
        let mut visited = BTreeSet::new();
        let mut stack = VecDeque::new();
        stack.push_back(owner_tid);
        while let Some(tid) = stack.pop_back() {
            if tid == start_tid {
                return true;
            }
            if !visited.insert(tid) {
                continue;
            }
            if let Some(wait_mutex) = self.waiting.get(&tid) {
                if let Some(Some(next_owner)) = self.owners.get(*wait_mutex) {
                    stack.push_back(*next_owner);
                }
            }
        }
        false
    }

    fn after_lock(&mut self, tid: usize, mutex_id: usize) {
        self.ensure(mutex_id);
        self.waiting.remove(&tid);
        self.owners[mutex_id] = Some(tid);
    }

    fn unlock(&mut self, tid: usize, mutex_id: usize) {
        if mutex_id < self.owners.len() && self.owners[mutex_id] == Some(tid) {
            self.owners[mutex_id] = None;
        }
        self.waiting.remove(&tid);
    }

    fn cleanup_thread(&mut self, tid: usize) {
        self.waiting.remove(&tid);
        for owner in self.owners.iter_mut() {
            if owner.map_or(false, |o| o == tid) {
                *owner = None;
            }
        }
    }
}

#[derive(Default)]
struct SemaphoreDeadlockState {
    available: Vec<isize>,
    allocation: BTreeMap<usize, Vec<usize>>,
    waiting: BTreeMap<usize, usize>,
}

impl SemaphoreDeadlockState {
    fn ensure(&mut self, sem_id: usize) {
        let target_len = sem_id + 1;
        if self.available.len() < target_len {
            self.available.resize(target_len, 0);
            for alloc in self.allocation.values_mut() {
                alloc.resize(target_len, 0);
            }
        }
    }

    fn register(&mut self, sem_id: usize, initial: usize) {
        self.ensure(sem_id);
        if let Some(slot) = self.available.get_mut(sem_id) {
            *slot = initial as isize;
        }
        for alloc in self.allocation.values_mut() {
            alloc[sem_id] = 0;
        }
        self.waiting.retain(|_, wait_sem| *wait_sem != sem_id);
    }

    fn ensure_alloc_entry(&mut self, tid: usize) -> &mut Vec<usize> {
        let len = self.available.len();
        let entry = self.allocation.entry(tid).or_insert_with(|| vec![0; len]);
        if entry.len() < len {
            entry.resize(len, 0);
        }
        entry
    }

    fn before_down(&mut self, enabled: bool, tid: usize, sem_id: usize) -> Result<bool, ()> {
        self.ensure(sem_id);
        self.waiting.remove(&tid);
        if self.available[sem_id] > 0 {
            return Ok(true);
        }
        if enabled && self.would_deadlock(tid, sem_id) {
            return Err(());
        }
        self.waiting.insert(tid, sem_id);
        Ok(false)
    }

    fn would_deadlock(&self, tid: usize, sem_id: usize) -> bool {
        let resource_types = self.available.len();
        if sem_id >= resource_types {
            return false;
        }

        let mut tids = BTreeSet::new();
        tids.extend(self.allocation.keys().cloned());
        tids.extend(self.waiting.keys().cloned());
        tids.insert(tid);

        if tids.is_empty() {
            return false;
        }

        let mut work = self.available.clone();

        let mut allocation_snapshot: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
        for id in &tids {
            let mut alloc_vec = vec![0; resource_types];
            if let Some(original) = self.allocation.get(id) {
                for (idx, &val) in original.iter().enumerate().take(resource_types) {
                    alloc_vec[idx] = val;
                }
            }
            allocation_snapshot.insert(*id, alloc_vec);
        }

        let mut request_snapshot: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
        for id in &tids {
            request_snapshot.insert(*id, vec![0; resource_types]);
        }
        for (wait_tid, wait_sem) in self.waiting.iter() {
            if *wait_sem < resource_types {
                if let Some(req_vec) = request_snapshot.get_mut(wait_tid) {
                    req_vec[*wait_sem] = req_vec[*wait_sem].saturating_add(1);
                }
            }
        }
        if let Some(req_vec) = request_snapshot.get_mut(&tid) {
            req_vec[sem_id] = req_vec[sem_id].saturating_add(1);
        }

        let mut finish: BTreeMap<usize, bool> = BTreeMap::new();
        for id in &tids {
            let has_allocation = allocation_snapshot
                .get(id)
                .map(|v| v.iter().any(|&val| val > 0))
                .unwrap_or(false);
            let has_request = request_snapshot
                .get(id)
                .map(|v| v.iter().any(|&val| val > 0))
                .unwrap_or(false);
            finish.insert(*id, !has_allocation && !has_request);
        }

        loop {
            let mut progressed = false;
            for id in tids.iter() {
                if *finish.get(id).unwrap() {
                    continue;
                }
                let req_vec = &request_snapshot[id];
                if req_vec.iter().enumerate().all(|(idx, &need)| {
                    need == 0 || (idx < work.len() && (need as isize) <= work[idx])
                }) {
                    if let Some(alloc_vec) = allocation_snapshot.get(id) {
                        for (idx, &val) in alloc_vec.iter().enumerate() {
                            if idx < work.len() {
                                work[idx] += val as isize;
                            }
                        }
                    }
                    finish.insert(*id, true);
                    progressed = true;
                }
            }
            if !progressed {
                break;
            }
        }

        finish.values().any(|done| !done)
    }

    fn after_down(&mut self, tid: usize, sem_id: usize, _immediate: bool) {
        self.ensure(sem_id);
        self.waiting.remove(&tid);
        self.available[sem_id] -= 1;
        let alloc = self.ensure_alloc_entry(tid);
        alloc[sem_id] += 1;
    }

    fn up(&mut self, tid: usize, sem_id: usize) {
        self.ensure(sem_id);
        if let Some(slot) = self.available.get_mut(sem_id) {
            *slot += 1;
        }
        let mut remove_entry = false;
        if let Some(alloc) = self.allocation.get_mut(&tid) {
            if sem_id < alloc.len() && alloc[sem_id] > 0 {
                alloc[sem_id] -= 1;
            }
            if alloc.iter().all(|&c| c == 0) {
                remove_entry = true;
            }
        }
        if remove_entry {
            self.allocation.remove(&tid);
        }
    }

    fn cleanup_thread(&mut self, tid: usize) {
        if let Some(alloc) = self.allocation.remove(&tid) {
            for (sem_id, count) in alloc.into_iter().enumerate() {
                if count > 0 {
                    self.ensure(sem_id);
                    if let Some(slot) = self.available.get_mut(sem_id) {
                        *slot += count as isize;
                    }
                }
            }
        }
        self.waiting.remove(&tid);
    }
}

/// Process Control Block
pub struct ProcessControlBlock {
    /// immutable
    pub pid: PidHandle,
    /// mutable
    inner: UPSafeCell<ProcessControlBlockInner>,
}

/// Inner of Process Control Block
pub struct ProcessControlBlockInner {
    /// is zombie?
    pub is_zombie: bool,
    /// memory set(address space)
    pub memory_set: MemorySet,
    /// parent process
    pub parent: Option<Weak<ProcessControlBlock>>,
    /// children process
    pub children: Vec<Arc<ProcessControlBlock>>,
    /// exit code
    pub exit_code: i32,
    /// file descriptor table
    pub fd_table: Vec<Option<Arc<dyn File + Send + Sync>>>,
    /// signal flags
    pub signals: SignalFlags,
    /// tasks(also known as threads)
    pub tasks: Vec<Option<Arc<TaskControlBlock>>>,
    /// task resource allocator
    pub task_res_allocator: RecycleAllocator,
    /// mutex list
    pub mutex_list: Vec<Option<Arc<dyn Mutex>>>,
    /// semaphore list
    pub semaphore_list: Vec<Option<Arc<Semaphore>>>,
    /// condvar list
    pub condvar_list: Vec<Option<Arc<Condvar>>>,
    /// deadlock detection state
    pub deadlock: DeadlockState,
}

impl ProcessControlBlockInner {
    #[allow(unused)]
    /// get the address of app's page table
    pub fn get_user_token(&self) -> usize {
        self.memory_set.token()
    }
    /// allocate a new file descriptor
    pub fn alloc_fd(&mut self) -> usize {
        if let Some(fd) = (0..self.fd_table.len()).find(|fd| self.fd_table[*fd].is_none()) {
            fd
        } else {
            self.fd_table.push(None);
            self.fd_table.len() - 1
        }
    }
    /// allocate a new task id
    pub fn alloc_tid(&mut self) -> usize {
        self.task_res_allocator.alloc()
    }
    /// deallocate a task id
    pub fn dealloc_tid(&mut self, tid: usize) {
        self.task_res_allocator.dealloc(tid)
    }
    /// the count of tasks(threads) in this process
    pub fn thread_count(&self) -> usize {
        self.tasks.len()
    }
    /// get a task with tid in this process
    pub fn get_task(&self, tid: usize) -> Arc<TaskControlBlock> {
        self.tasks[tid].as_ref().unwrap().clone()
    }
}

impl ProcessControlBlock {
    /// inner_exclusive_access
    pub fn inner_exclusive_access(&self) -> RefMut<'_, ProcessControlBlockInner> {
        self.inner.exclusive_access()
    }
    /// new process from elf file
    pub fn new(elf_data: &[u8]) -> Arc<Self> {
        trace!("kernel: ProcessControlBlock::new");
        // memory_set with elf program headers/trampoline/trap context/user stack
        let (memory_set, ustack_base, entry_point) = MemorySet::from_elf(elf_data);
        // allocate a pid
        let pid_handle = pid_alloc();
        let process = Arc::new(Self {
            pid: pid_handle,
            inner: unsafe {
                UPSafeCell::new(ProcessControlBlockInner {
                    is_zombie: false,
                    memory_set,
                    parent: None,
                    children: Vec::new(),
                    exit_code: 0,
                    fd_table: vec![
                        // 0 -> stdin
                        Some(Arc::new(Stdin)),
                        // 1 -> stdout
                        Some(Arc::new(Stdout)),
                        // 2 -> stderr
                        Some(Arc::new(Stdout)),
                    ],
                    signals: SignalFlags::empty(),
                    tasks: Vec::new(),
                    task_res_allocator: RecycleAllocator::new(),
                    mutex_list: Vec::new(),
                    semaphore_list: Vec::new(),
                    condvar_list: Vec::new(),
                    deadlock: DeadlockState::new(),
                })
            },
        });
        // create a main thread, we should allocate ustack and trap_cx here
        let task = Arc::new(TaskControlBlock::new(
            Arc::clone(&process),
            ustack_base,
            true,
        ));
        // prepare trap_cx of main thread
        let task_inner = task.inner_exclusive_access();
        let trap_cx = task_inner.get_trap_cx();
        let ustack_top = task_inner.res.as_ref().unwrap().ustack_top();
        let kstack_top = task.kstack.get_top();
        drop(task_inner);
        *trap_cx = TrapContext::app_init_context(
            entry_point,
            ustack_top,
            KERNEL_SPACE.exclusive_access().token(),
            kstack_top,
            trap_handler as usize,
        );
        // add main thread to the process
        let mut process_inner = process.inner_exclusive_access();
        process_inner.tasks.push(Some(Arc::clone(&task)));
        drop(process_inner);
        insert_into_pid2process(process.getpid(), Arc::clone(&process));
        // add main thread to scheduler
        add_task(task);
        process
    }

    /// Only support processes with a single thread.
    pub fn exec(self: &Arc<Self>, elf_data: &[u8], args: Vec<String>) {
        trace!("kernel: exec");
        assert_eq!(self.inner_exclusive_access().thread_count(), 1);
        // memory_set with elf program headers/trampoline/trap context/user stack
        trace!("kernel: exec .. MemorySet::from_elf");
        let (memory_set, ustack_base, entry_point) = MemorySet::from_elf(elf_data);
        let new_token = memory_set.token();
        // substitute memory_set
        trace!("kernel: exec .. substitute memory_set");
        self.inner_exclusive_access().memory_set = memory_set;
        // then we alloc user resource for main thread again
        // since memory_set has been changed
        trace!("kernel: exec .. alloc user resource for main thread again");
        let task = self.inner_exclusive_access().get_task(0);
        let mut task_inner = task.inner_exclusive_access();
        task_inner.res.as_mut().unwrap().ustack_base = ustack_base;
        task_inner.res.as_mut().unwrap().alloc_user_res();
        task_inner.trap_cx_ppn = task_inner.res.as_mut().unwrap().trap_cx_ppn();
        // push arguments on user stack
        trace!("kernel: exec .. push arguments on user stack");
        let mut user_sp = task_inner.res.as_mut().unwrap().ustack_top();
        user_sp -= (args.len() + 1) * core::mem::size_of::<usize>();
        let argv_base = user_sp;
        let mut argv: Vec<_> = (0..=args.len())
            .map(|arg| {
                translated_refmut(
                    new_token,
                    (argv_base + arg * core::mem::size_of::<usize>()) as *mut usize,
                )
            })
            .collect();
        *argv[args.len()] = 0;
        for i in 0..args.len() {
            user_sp -= args[i].len() + 1;
            *argv[i] = user_sp;
            let mut p = user_sp;
            for c in args[i].as_bytes() {
                *translated_refmut(new_token, p as *mut u8) = *c;
                p += 1;
            }
            *translated_refmut(new_token, p as *mut u8) = 0;
        }
        // make the user_sp aligned to 8B for k210 platform
        user_sp -= user_sp % core::mem::size_of::<usize>();
        // initialize trap_cx
        trace!("kernel: exec .. initialize trap_cx");
        let mut trap_cx = TrapContext::app_init_context(
            entry_point,
            user_sp,
            KERNEL_SPACE.exclusive_access().token(),
            task.kstack.get_top(),
            trap_handler as usize,
        );
        trap_cx.x[10] = args.len();
        trap_cx.x[11] = argv_base;
        *task_inner.get_trap_cx() = trap_cx;
    }

    /// Only support processes with a single thread.
    pub fn fork(self: &Arc<Self>) -> Arc<Self> {
        trace!("kernel: fork");
        let mut parent = self.inner_exclusive_access();
        assert_eq!(parent.thread_count(), 1);
        // clone parent's memory_set completely including trampoline/ustacks/trap_cxs
        let memory_set = MemorySet::from_existed_user(&parent.memory_set);
        // alloc a pid
        let pid = pid_alloc();
        // copy fd table
        let mut new_fd_table: Vec<Option<Arc<dyn File + Send + Sync>>> = Vec::new();
        for fd in parent.fd_table.iter() {
            if let Some(file) = fd {
                new_fd_table.push(Some(file.clone()));
            } else {
                new_fd_table.push(None);
            }
        }
        // create child process pcb
        let child = Arc::new(Self {
            pid,
            inner: unsafe {
                UPSafeCell::new(ProcessControlBlockInner {
                    is_zombie: false,
                    memory_set,
                    parent: Some(Arc::downgrade(self)),
                    children: Vec::new(),
                    exit_code: 0,
                    fd_table: new_fd_table,
                    signals: SignalFlags::empty(),
                    tasks: Vec::new(),
                    task_res_allocator: RecycleAllocator::new(),
                    mutex_list: Vec::new(),
                    semaphore_list: Vec::new(),
                    condvar_list: Vec::new(),
                    deadlock: DeadlockState::new(),
                })
            },
        });
        // add child
        parent.children.push(Arc::clone(&child));
        // create main thread of child process
        let task = Arc::new(TaskControlBlock::new(
            Arc::clone(&child),
            parent
                .get_task(0)
                .inner_exclusive_access()
                .res
                .as_ref()
                .unwrap()
                .ustack_base(),
            // here we do not allocate trap_cx or ustack again
            // but mention that we allocate a new kstack here
            false,
        ));
        // attach task to child process
        let mut child_inner = child.inner_exclusive_access();
        child_inner.tasks.push(Some(Arc::clone(&task)));
        drop(child_inner);
        // modify kstack_top in trap_cx of this thread
        let task_inner = task.inner_exclusive_access();
        let trap_cx = task_inner.get_trap_cx();
        trap_cx.kernel_sp = task.kstack.get_top();
        drop(task_inner);
        insert_into_pid2process(child.getpid(), Arc::clone(&child));
        // add this thread to scheduler
        add_task(task);
        child
    }
    /// get pid
    pub fn getpid(&self) -> usize {
        self.pid.0
    }
}
