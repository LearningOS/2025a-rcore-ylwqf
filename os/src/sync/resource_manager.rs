//! Resource allocation bookkeeping and deadlock detection helpers.

use alloc::vec;
use alloc::vec::Vec;

/// Summarises the result of a deadlock detection pass.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeadlockState {
    /// All processes can eventually proceed.
    Safe,
    /// Processes listed in the vector are deadlocked.
    Deadlocked(Vec<usize>),
}

/// Stores per-resource totals together with allocation/need matrices.
#[derive(Clone, Debug, Default)]
pub struct ResourceManager {
    totals: Vec<usize>,
    allocation: Vec<Vec<usize>>,
    need: Vec<Vec<usize>>,
}

impl ResourceManager {
    /// Create an empty manager.
    pub fn new() -> Self {
        Self {
            totals: Vec::new(),
            allocation: Vec::new(),
            need: Vec::new(),
        }
    }

    /// Return number of tracked resource kinds.
    pub fn resource_count(&self) -> usize {
        self.totals.len()
    }

    /// Return number of tracked processes (rows).
    pub fn process_count(&self) -> usize {
        self.allocation.len()
    }

    /// Ensure we have rows up to and including `pid`.
    pub fn ensure_process(&mut self, pid: usize) {
        while self.allocation.len() <= pid {
            self.allocation.push(vec![0; self.resource_count()]);
            self.need.push(vec![0; self.resource_count()]);
        }
    }

    /// Add a new resource kind with given total units. Returns its index.
    pub fn add_resource(&mut self, total: usize) -> usize {
        self.totals.push(total);
        for row in &mut self.allocation {
            row.push(0);
        }
        for row in &mut self.need {
            row.push(0);
        }
        self.totals.len() - 1
    }

    /// Reset allocation/need for the resource and update its total units.
    pub fn reset_resource(&mut self, res_id: usize, total: usize) {
        self.totals[res_id] = total;
        for row in &mut self.allocation {
            if let Some(cell) = row.get_mut(res_id) {
                *cell = 0;
            }
        }
        for row in &mut self.need {
            if let Some(cell) = row.get_mut(res_id) {
                *cell = 0;
            }
        }
    }

    /// Set allocation for a process/resource pair.
    pub fn set_allocation(&mut self, pid: usize, res_id: usize, value: usize) {
        self.ensure_process(pid);
        self.allocation[pid][res_id] = value;
    }

    /// Add to allocation count (used when acquiring semaphore units).
    pub fn add_allocation(&mut self, pid: usize, res_id: usize, delta: usize) {
        self.ensure_process(pid);
        let cell = &mut self.allocation[pid][res_id];
        *cell = cell.saturating_add(delta);
    }

    /// Subtract from allocation count when releasing semaphore units.
    pub fn sub_allocation(&mut self, pid: usize, res_id: usize, delta: usize) {
        self.ensure_process(pid);
        let cell = &mut self.allocation[pid][res_id];
        *cell = cell.saturating_sub(delta);
    }

    /// Sum allocations across all processes for the resource.
    pub fn total_allocation(&self, res_id: usize) -> usize {
        self.allocation
            .iter()
            .map(|row| *row.get(res_id).unwrap_or(&0))
            .sum()
    }

    /// Set outstanding need for process/resource.
    pub fn set_need(&mut self, pid: usize, res_id: usize, value: usize) {
        self.ensure_process(pid);
        self.need[pid][res_id] = value;
    }

    /// Clear all outstanding needs.
    pub fn clear_all_needs(&mut self) {
        for row in &mut self.need {
            for cell in row {
                *cell = 0;
            }
        }
    }

    /// Return the available units for a resource.
    pub fn available_units(&self, res_id: usize) -> usize {
        self.totals[res_id].saturating_sub(self.total_allocation(res_id))
    }

    /// Compute the availability vector for all resources.
    fn available(&self) -> Vec<usize> {
        self.totals
            .iter()
            .enumerate()
            .map(|(idx, total)| total.saturating_sub(self.total_allocation(idx)))
            .collect()
    }

    /// Run a deadlock detection cycle and return the result.
    pub fn detect_deadlock(&self) -> DeadlockState {
        let mut work = self.available();
        let resource_kinds = self.resource_count();
        let mut finish = vec![false; self.process_count()];

        loop {
            let mut progress = false;
            for pid in 0..self.process_count() {
                if finish[pid] {
                    continue;
                }
                let need_row = &self.need[pid];
                if need_row
                    .iter()
                    .zip(work.iter())
                    .all(|(need, avail)| *need <= *avail)
                {
                    for idx in 0..resource_kinds {
                        work[idx] += self.allocation[pid][idx];
                    }
                    finish[pid] = true;
                    progress = true;
                }
            }
            if !progress {
                break;
            }
        }

        let mut deadlocked = Vec::new();
        for (pid, done) in finish.into_iter().enumerate() {
            if !done {
                deadlocked.push(pid);
            }
        }

        if deadlocked.is_empty() {
            DeadlockState::Safe
        } else {
            DeadlockState::Deadlocked(deadlocked)
        }
    }
}

/// Tracks resources that are identified by indices in user space (mutex id, semaphore id).
#[derive(Clone, Debug, Default)]
pub struct ResourceTracker {
    manager: ResourceManager,
    mapping: Vec<Option<usize>>, // user-visible id -> internal resource id
}

impl ResourceTracker {
    /// Create an empty tracker.
    pub fn new() -> Self {
        Self {
            manager: ResourceManager::new(),
            mapping: Vec::new(),
        }
    }

    /// Ensure a mapping entry exists.
    fn ensure_entry(&mut self, external_id: usize) {
        if self.mapping.len() <= external_id {
            self.mapping.resize(external_id + 1, None);
        }
    }

    /// Register (or reset) a resource and return its internal id.
    pub fn register(&mut self, external_id: usize, total: usize) -> usize {
        self.ensure_entry(external_id);
        if let Some(res_id) = self.mapping[external_id] {
            self.manager.reset_resource(res_id, total);
            res_id
        } else {
            let res_id = self.manager.add_resource(total);
            self.mapping[external_id] = Some(res_id);
            res_id
        }
    }

    /// Look up the internal resource id.
    pub fn resource_id(&self, external_id: usize) -> Option<usize> {
        self.mapping.get(external_id).and_then(|id| *id)
    }

    /// Ensure process row exists.
    pub fn ensure_process(&mut self, pid: usize) {
        self.manager.ensure_process(pid);
    }

    /// Available units for resource. Panics if resource missing.
    pub fn available_units(&self, res_id: usize) -> usize {
        self.manager.available_units(res_id)
    }

    /// Set outstanding need.
    pub fn set_need(&mut self, pid: usize, res_id: usize, value: usize) {
        self.manager.set_need(pid, res_id, value);
    }

    /// Clear all needs across resources.
    pub fn clear_all_needs(&mut self) {
        self.manager.clear_all_needs();
    }

    /// Increase allocation for a process/resource pair.
    pub fn add_allocation(&mut self, pid: usize, res_id: usize, delta: usize) {
        self.manager.add_allocation(pid, res_id, delta);
    }

    /// Set allocation explicitly (used by mutex logic).
    pub fn set_allocation(&mut self, pid: usize, res_id: usize, value: usize) {
        self.manager.set_allocation(pid, res_id, value);
    }

    /// Decrease allocation when releasing a resource.
    pub fn sub_allocation(&mut self, pid: usize, res_id: usize, delta: usize) {
        self.manager.sub_allocation(pid, res_id, delta);
    }

    /// Access underlying manager for detection.
    pub fn detect_deadlock(&self) -> DeadlockState {
        self.manager.detect_deadlock()
    }
}

/// Per-process controller to toggle deadlock detection and bookkeep resources.
#[derive(Clone, Debug, Default)]
pub struct DeadlockController {
    enabled: bool,
    mutex_tracker: ResourceTracker,
    semaphore_tracker: ResourceTracker,
}

impl DeadlockController {
    /// Create a new controller with detection disabled.
    pub fn new() -> Self {
        Self {
            enabled: false,
            mutex_tracker: ResourceTracker::new(),
            semaphore_tracker: ResourceTracker::new(),
        }
    }

    /// Query whether detection is enabled.
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Enable or disable detection. When disabling, outstanding needs are cleared.
    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
        if !enabled {
            self.mutex_tracker.clear_all_needs();
            self.semaphore_tracker.clear_all_needs();
        }
    }

    /// Ensure per-thread rows exist for tid.
    pub fn ensure_thread(&mut self, tid: usize) {
        self.mutex_tracker.ensure_process(tid);
        self.semaphore_tracker.ensure_process(tid);
    }

    /// Register (or reset) a mutex resource. Returns internal resource id.
    pub fn register_mutex(&mut self, mutex_id: usize) -> usize {
        self.mutex_tracker.register(mutex_id, 1)
    }

    /// Register (or reset) a semaphore resource. Returns internal resource id.
    pub fn register_semaphore(&mut self, sem_id: usize, total: usize) -> usize {
        self.semaphore_tracker.register(sem_id, total)
    }

    /// Prepare to lock a mutex. Returns Err if invalid id or deadlock detected.
    pub fn before_mutex_lock(&mut self, tid: usize, mutex_id: usize) -> Result<(), isize> {
        self.ensure_thread(tid);
        let res_id = self.mutex_tracker.resource_id(mutex_id).ok_or(-1_isize)?;
        self.mutex_tracker.set_need(tid, res_id, 0);
        if !self.enabled {
            return Ok(());
        }
        if self.mutex_tracker.available_units(res_id) > 0 {
            return Ok(());
        }
        self.mutex_tracker.set_need(tid, res_id, 1);
        if let DeadlockState::Deadlocked(_) = self.mutex_tracker.detect_deadlock() {
            self.mutex_tracker.set_need(tid, res_id, 0);
            return Err(-0xDEAD);
        }
        Ok(())
    }

    /// Record that the mutex lock has been acquired.
    pub fn after_mutex_lock(&mut self, tid: usize, mutex_id: usize) -> Result<(), isize> {
        self.ensure_thread(tid);
        let res_id = self.mutex_tracker.resource_id(mutex_id).ok_or(-1_isize)?;
        self.mutex_tracker.set_need(tid, res_id, 0);
        self.mutex_tracker.set_allocation(tid, res_id, 1);
        Ok(())
    }

    /// Record that the mutex has been unlocked.
    pub fn after_mutex_unlock(&mut self, tid: usize, mutex_id: usize) -> Result<(), isize> {
        self.ensure_thread(tid);
        let res_id = self.mutex_tracker.resource_id(mutex_id).ok_or(-1_isize)?;
        self.mutex_tracker.set_need(tid, res_id, 0);
        self.mutex_tracker.set_allocation(tid, res_id, 0);
        Ok(())
    }

    /// Prepare to perform semaphore down. Returns Err if invalid id or deadlock detected.
    pub fn before_semaphore_down(&mut self, tid: usize, sem_id: usize) -> Result<(), isize> {
        self.ensure_thread(tid);
        let res_id = self.semaphore_tracker.resource_id(sem_id).ok_or(-1_isize)?;
        self.semaphore_tracker.set_need(tid, res_id, 0);
        if !self.enabled {
            return Ok(());
        }
        if self.semaphore_tracker.available_units(res_id) > 0 {
            return Ok(());
        }
        self.semaphore_tracker.set_need(tid, res_id, 1);
        if let DeadlockState::Deadlocked(_) = self.semaphore_tracker.detect_deadlock() {
            self.semaphore_tracker.set_need(tid, res_id, 0);
            return Err(-0xDEAD);
        }
        Ok(())
    }

    /// Record completion of semaphore down.
    pub fn after_semaphore_down(&mut self, tid: usize, sem_id: usize) -> Result<(), isize> {
        self.ensure_thread(tid);
        let res_id = self.semaphore_tracker.resource_id(sem_id).ok_or(-1_isize)?;
        self.semaphore_tracker.set_need(tid, res_id, 0);
        self.semaphore_tracker.add_allocation(tid, res_id, 1);
        Ok(())
    }

    /// Record completion of semaphore up (release one unit).
    pub fn after_semaphore_up(&mut self, tid: usize, sem_id: usize) -> Result<(), isize> {
        self.ensure_thread(tid);
        let res_id = self.semaphore_tracker.resource_id(sem_id).ok_or(-1_isize)?;
        self.semaphore_tracker.sub_allocation(tid, res_id, 1);
        Ok(())
    }

    /// Remove bookkeeping entries related to a thread that is exiting.
    pub fn cleanup_thread(&mut self, tid: usize) {
        if tid < self.mutex_tracker.manager.process_count() {
            if let Some(row) = self.mutex_tracker.manager.allocation.get_mut(tid) {
                row.fill(0);
            }
            if let Some(row) = self.mutex_tracker.manager.need.get_mut(tid) {
                row.fill(0);
            }
        }
        if tid < self.semaphore_tracker.manager.process_count() {
            if let Some(row) = self.semaphore_tracker.manager.allocation.get_mut(tid) {
                row.fill(0);
            }
            if let Some(row) = self.semaphore_tracker.manager.need.get_mut(tid) {
                row.fill(0);
            }
        }
    }
}
