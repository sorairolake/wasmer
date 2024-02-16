#[cfg(feature = "journal")]
use crate::{journal::JournalEffector, unwind, WasiResult};
use crate::{
    journal::SnapshotTrigger, runtime::module_cache::ModuleHash, VirtualTaskManager, WasiEnv,
    WasiRuntimeError,
};
use futures::{Future, FutureExt};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    convert::TryInto,
    pin::Pin,
    sync::{
        atomic::{AtomicU32, Ordering},
        Arc, Condvar, Mutex, MutexGuard, RwLock, Weak,
    },
    task::{Context, Poll, Waker},
    time::Duration,
};
use tracing::trace;
use wasmer::FunctionEnvMut;
use wasmer_wasix_types::{
    types::Signal,
    wasi::{Errno, ExitCode, Snapshot0Clockid},
};

use crate::{
    os::task::signal::WasiSignalInterval, syscalls::platform_clock_time_get, WasiThread,
    WasiThreadHandle, WasiThreadId,
};

use super::{
    control_plane::{ControlPlaneError, WasiControlPlaneHandle},
    signal::{SignalDeliveryError, SignalHandlerAbi},
    task_join_handle::OwnedTaskStatus,
    thread::WasiMemoryLayout,
};

/// Represents the ID of a sub-process
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct WasiProcessId(u32);

impl WasiProcessId {
    pub fn raw(&self) -> u32 {
        self.0
    }
}

impl From<i32> for WasiProcessId {
    fn from(id: i32) -> Self {
        Self(id as u32)
    }
}

impl From<WasiProcessId> for i32 {
    fn from(val: WasiProcessId) -> Self {
        val.0 as i32
    }
}

impl From<u32> for WasiProcessId {
    fn from(id: u32) -> Self {
        Self(id)
    }
}

impl From<WasiProcessId> for u32 {
    fn from(val: WasiProcessId) -> Self {
        val.0
    }
}

impl std::fmt::Display for WasiProcessId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::fmt::Debug for WasiProcessId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

pub type LockableWasiProcessInner = Arc<(Mutex<WasiProcessInner>, Condvar)>;

/// Represents a process running within the compute state
/// TODO: fields should be private and only accessed via methods.
#[derive(Debug, Clone)]
pub struct WasiProcess {
    /// Unique ID of this process
    pub(crate) pid: WasiProcessId,
    /// Hash of the module that this process is using
    pub(crate) module_hash: ModuleHash,
    /// List of all the children spawned from this thread
    pub(crate) parent: Option<Weak<RwLock<WasiProcessInner>>>,
    /// The inner protected region of the process with a conditional
    /// variable that is used for coordination such as checksums.
    pub(crate) inner: LockableWasiProcessInner,
    /// Reference back to the compute engine
    // TODO: remove this reference, access should happen via separate state instead
    // (we don't want cyclical references)
    pub(crate) compute: WasiControlPlaneHandle,
    /// Reference to the exit code for the main thread
    pub(crate) finished: Arc<OwnedTaskStatus>,
    /// Number of threads waiting for children to exit
    pub(crate) waiting: Arc<AtomicU32>,
    /// Number of tokens that are currently active and thus
    /// the exponential backoff of CPU is halted (as in CPU
    /// is allowed to run freely)
    pub(crate) cpu_run_tokens: Arc<AtomicU32>,
}

/// Represents a freeze of all threads to perform some action
/// on the total state-machine. This is normally done for
/// things like snapshots which require the memory to remain
/// stable while it performs a diff.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum WasiProcessCheckpoint {
    /// No checkpoint will take place and the process
    /// should just execute as per normal
    Execute,
    /// The process needs to take a snapshot of the
    /// memory and state-machine
    Snapshot { trigger: SnapshotTrigger },
}

// TODO: fields should be private and only accessed via methods.
#[derive(Debug)]
pub struct WasiProcessInner {
    /// Unique ID of this process
    pub pid: WasiProcessId,
    /// The threads that make up this process
    pub threads: HashMap<WasiThreadId, WasiThread>,
    /// Number of threads running for this process
    pub thread_count: u32,
    /// Signals that will be triggered at specific intervals
    pub signal_intervals: HashMap<Signal, WasiSignalInterval>,
    /// List of all the children spawned from this thread
    pub children: Vec<WasiProcess>,
    /// Represents a checkpoint which blocks all the threads
    /// and then executes some maintenance action
    pub checkpoint: WasiProcessCheckpoint,
    /// Referenced list of wakers that will be triggered
    /// when the process goes active again due to a token
    /// being acquired
    cpu_backoff_wakers: HashMap<u64, Waker>,
    /// Seed used to register CPU release wakers
    cpu_backoff_waker_seed: u64,
    /// The amount of CPU backoff time we are currently waiting
    cpu_backoff_time: Duration,
    /// When the backoff is reset the cool-off period will keep
    /// things running for a short period of time extra
    cpu_run_cool_off: u128,
    /// Maximum amount of CPU backoff time before it starts capping
    max_cpu_backoff_time: Duration,
    /// Amount of time the CPU should cool-off after exiting run
    /// before it begins a backoff
    max_cpu_cool_off_time: Duration,
}

pub enum MaybeCheckpointResult<'a> {
    NotThisTime(FunctionEnvMut<'a, WasiEnv>),
    Unwinding,
}

impl WasiProcessInner {
    /// Checkpoints the process which will cause all other threads to
    /// pause and for the thread and memory state to be saved
    #[cfg(feature = "journal")]
    pub fn checkpoint<M: wasmer_types::MemorySize>(
        inner: LockableWasiProcessInner,
        ctx: FunctionEnvMut<'_, WasiEnv>,
        for_what: WasiProcessCheckpoint,
    ) -> WasiResult<MaybeCheckpointResult<'_>> {
        // Set the checkpoint flag and then enter the normal processing loop
        {
            let mut inner = inner.0.lock().unwrap();
            inner.checkpoint = for_what;
        }

        Self::maybe_checkpoint::<M>(inner, ctx)
    }

    /// If a checkpoint has been started this will block the current process
    /// until the checkpoint operation has completed
    #[cfg(feature = "journal")]
    pub fn maybe_checkpoint<M: wasmer_types::MemorySize>(
        inner: LockableWasiProcessInner,
        ctx: FunctionEnvMut<'_, WasiEnv>,
    ) -> WasiResult<MaybeCheckpointResult<'_>> {
        // Enter the lock which will determine if we are in a checkpoint or not

        use bytes::Bytes;
        use wasmer::AsStoreMut;
        use wasmer_types::OnCalledAction;

        use crate::{rewind_ext, WasiError};
        let guard = inner.0.lock().unwrap();
        if guard.checkpoint == WasiProcessCheckpoint::Execute {
            // No checkpoint so just carry on
            return Ok(Ok(MaybeCheckpointResult::NotThisTime(ctx)));
        }
        trace!("checkpoint capture");
        drop(guard);

        // Perform the unwind action
        unwind::<M, _>(ctx, move |mut ctx, memory_stack, rewind_stack| {
            // Grab all the globals and serialize them
            let store_data =
                crate::utils::store::capture_instance_snapshot(&mut ctx.as_store_mut())
                    .serialize()
                    .unwrap();
            let memory_stack = memory_stack.freeze();
            let rewind_stack = rewind_stack.freeze();
            let store_data = Bytes::from(store_data);

            tracing::debug!(
                "stack snapshot unwind (memory_stack={}, rewind_stack={}, store_data={})",
                memory_stack.len(),
                rewind_stack.len(),
                store_data.len(),
            );

            // Write our thread state to the snapshot
            let tid = ctx.data().thread.tid();
            if let Err(err) = JournalEffector::save_thread_state::<M>(
                &mut ctx,
                tid,
                memory_stack.clone(),
                rewind_stack.clone(),
                store_data.clone(),
            ) {
                return wasmer_types::OnCalledAction::Trap(err.into());
            }

            let mut guard = inner.0.lock().unwrap();

            // Wait for the checkpoint to finish (or if we are the last thread
            // to freeze then we have to execute the checksum operation)
            loop {
                if let WasiProcessCheckpoint::Snapshot { trigger } = guard.checkpoint {
                    ctx.data().thread.set_check_pointing(true);

                    // Now if we are the last thread we also write the memory
                    let is_last_thread = guard.threads.values().all(WasiThread::is_check_pointing);
                    if is_last_thread {
                        if let Err(err) =
                            JournalEffector::save_memory_and_snapshot(&mut ctx, &mut guard, trigger)
                        {
                            inner.1.notify_all();
                            return wasmer_types::OnCalledAction::Trap(err.into());
                        }

                        // Clear the checkpointing flag and notify everyone to wake up
                        ctx.data().thread.set_check_pointing(false);
                        guard.checkpoint = WasiProcessCheckpoint::Execute;
                        trace!("checkpoint complete");
                        inner.1.notify_all();
                    } else {
                        guard = inner.1.wait(guard).unwrap();
                    }
                    continue;
                }

                ctx.data().thread.set_check_pointing(false);
                trace!("checkpoint finished");

                // Rewind the stack and carry on
                return match rewind_ext::<M>(&mut ctx, memory_stack, rewind_stack, store_data, None)
                {
                    Errno::Success => OnCalledAction::InvokeAgain,
                    err => {
                        tracing::warn!(
                            "snapshot resumption failed - could not rewind the stack - errno={}",
                            err
                        );
                        OnCalledAction::Trap(Box::new(WasiError::Exit(err.into())))
                    }
                };
            }
        })?;

        Ok(Ok(MaybeCheckpointResult::Unwinding))
    }
}

// TODO: why do we need this, how is it used?
pub(crate) struct WasiProcessWait {
    waiting: Arc<AtomicU32>,
}

impl WasiProcessWait {
    pub fn new(process: &WasiProcess) -> Self {
        process.waiting.fetch_add(1, Ordering::AcqRel);
        Self {
            waiting: process.waiting.clone(),
        }
    }
}

impl Drop for WasiProcessWait {
    fn drop(&mut self) {
        self.waiting.fetch_sub(1, Ordering::AcqRel);
    }
}

impl WasiProcess {
    pub fn new(pid: WasiProcessId, module_hash: ModuleHash, plane: WasiControlPlaneHandle) -> Self {
        let max_cpu_backoff_time = plane
            .upgrade()
            .and_then(|p| p.config().enable_exponential_cpu_backoff)
            .unwrap_or(Duration::from_secs(30));
        let max_cpu_cool_off_time = Duration::from_millis(500);
        WasiProcess {
            pid,
            module_hash,
            parent: None,
            compute: plane,
            inner: Arc::new((
                Mutex::new(WasiProcessInner {
                    pid,
                    threads: Default::default(),
                    thread_count: Default::default(),
                    signal_intervals: Default::default(),
                    children: Default::default(),
                    checkpoint: WasiProcessCheckpoint::Execute,
                    cpu_backoff_wakers: Default::default(),
                    cpu_backoff_waker_seed: 0,
                    cpu_backoff_time: Duration::ZERO,
                    cpu_run_cool_off: 0,
                    max_cpu_backoff_time,
                    max_cpu_cool_off_time,
                }),
                Condvar::new(),
            )),
            finished: Arc::new(OwnedTaskStatus::default()),
            waiting: Arc::new(AtomicU32::new(0)),
            cpu_run_tokens: Arc::new(AtomicU32::new(0)),
        }
    }

    pub(super) fn set_pid(&mut self, pid: WasiProcessId) {
        self.pid = pid;
    }

    /// Gets the process ID of this process
    pub fn pid(&self) -> WasiProcessId {
        self.pid
    }

    /// Gets the process ID of the parent process
    pub fn ppid(&self) -> WasiProcessId {
        self.parent
            .iter()
            .filter_map(|parent| parent.upgrade())
            .map(|parent| parent.read().unwrap().pid)
            .next()
            .unwrap_or(WasiProcessId(0))
    }

    /// Gains access to the process internals
    // TODO: Make this private, all inner access should be exposed with methods.
    pub fn lock(&self) -> MutexGuard<'_, WasiProcessInner> {
        self.inner.0.lock().unwrap()
    }

    /// Creates a a thread and returns it
    pub fn new_thread(
        &self,
        layout: WasiMemoryLayout,
    ) -> Result<WasiThreadHandle, ControlPlaneError> {
        let control_plane = self.compute.must_upgrade();
        let task_count_guard = control_plane.register_task()?;

        // Determine if its the main thread or not
        let is_main = {
            let inner = self.inner.0.lock().unwrap();
            inner.thread_count == 0
        };

        // Generate a new process ID (this is because the process ID and thread ID
        // address space must not overlap in libc). For the main proecess the TID=PID
        let tid: WasiThreadId = if is_main {
            self.pid().raw().into()
        } else {
            let tid: u32 = control_plane.generate_id()?.into();
            tid.into()
        };

        // The wait finished should be the process version if its the main thread
        let mut inner = self.inner.0.lock().unwrap();
        let finished = if is_main {
            self.finished.clone()
        } else {
            Arc::new(OwnedTaskStatus::default())
        };

        // Insert the thread into the pool
        let ctrl = WasiThread::new(self.pid(), tid, is_main, finished, task_count_guard, layout);
        inner.threads.insert(tid, ctrl.clone());
        inner.thread_count += 1;

        Ok(WasiThreadHandle::new(ctrl, &self.inner))
    }

    /// Gets a reference to a particular thread
    pub fn get_thread(&self, tid: &WasiThreadId) -> Option<WasiThread> {
        let inner = self.inner.0.lock().unwrap();
        inner.threads.get(tid).cloned()
    }

    /// Signals a particular thread in the process
    pub fn signal_thread(&self, tid: &WasiThreadId, signal: Signal) {
        // Sometimes we will signal the process rather than the thread hence this libc hardcoded value
        let mut tid = tid.raw();
        if tid == 1073741823 {
            tid = self.pid().raw();
        }
        let tid: WasiThreadId = tid.into();

        let pid = self.pid();
        tracing::trace!(%pid, %tid, "signal-thread({:?})", signal);

        let inner = self.inner.0.lock().unwrap();
        if let Some(thread) = inner.threads.get(&tid) {
            thread.signal(signal);
        } else {
            trace!(
                "wasi[{}]::lost-signal(tid={}, sig={:?})",
                self.pid(),
                tid,
                signal
            );
        }
    }

    /// Signals all the threads in this process
    pub fn signal_process(&self, signal: Signal) {
        let pid = self.pid();
        tracing::trace!(%pid, "signal-process({:?})", signal);

        {
            let inner = self.inner.0.lock().unwrap();
            if self.waiting.load(Ordering::Acquire) > 0 {
                let mut triggered = false;
                for child in inner.children.iter() {
                    child.signal_process(signal);
                    triggered = true;
                }
                if triggered {
                    return;
                }
            }
        }
        let inner = self.inner.0.lock().unwrap();
        for thread in inner.threads.values() {
            thread.signal(signal);
        }
    }

    /// Signals one of the threads every interval
    pub fn signal_interval(&self, signal: Signal, interval: Option<Duration>, repeat: bool) {
        let mut inner = self.inner.0.lock().unwrap();

        let interval = match interval {
            None => {
                inner.signal_intervals.remove(&signal);
                return;
            }
            Some(a) => a,
        };

        let now = platform_clock_time_get(Snapshot0Clockid::Monotonic, 1_000_000).unwrap() as u128;
        inner.signal_intervals.insert(
            signal,
            WasiSignalInterval {
                signal,
                interval,
                last_signal: now,
                repeat,
            },
        );
    }

    /// Returns the number of active threads for this process
    pub fn active_threads(&self) -> u32 {
        let inner = self.inner.0.lock().unwrap();
        inner.thread_count
    }

    /// Waits until the process is finished.
    pub async fn join(&self) -> Result<ExitCode, Arc<WasiRuntimeError>> {
        let _guard = WasiProcessWait::new(self);
        self.finished.await_termination().await
    }

    /// Attempts to join on the process
    pub fn try_join(&self) -> Option<Result<ExitCode, Arc<WasiRuntimeError>>> {
        self.finished.status().into_finished()
    }

    /// Waits for all the children to be finished
    pub async fn join_children(&mut self) -> Option<Result<ExitCode, Arc<WasiRuntimeError>>> {
        let _guard = WasiProcessWait::new(self);
        let children: Vec<_> = {
            let inner = self.inner.0.lock().unwrap();
            inner.children.clone()
        };
        if children.is_empty() {
            return None;
        }
        let mut waits = Vec::new();
        for child in children {
            if let Some(process) = self.compute.must_upgrade().get_process(child.pid) {
                let inner = self.inner.clone();
                waits.push(async move {
                    let join = process.join().await;
                    let mut inner = inner.0.lock().unwrap();
                    inner.children.retain(|a| a.pid != child.pid);
                    join
                })
            }
        }
        futures::future::join_all(waits.into_iter())
            .await
            .into_iter()
            .next()
    }

    /// Waits for any of the children to finished
    pub async fn join_any_child(&mut self) -> Result<Option<(WasiProcessId, ExitCode)>, Errno> {
        let _guard = WasiProcessWait::new(self);
        let children: Vec<_> = {
            let inner = self.inner.0.lock().unwrap();
            inner.children.clone()
        };
        if children.is_empty() {
            return Err(Errno::Child);
        }

        let mut waits = Vec::new();
        for child in children {
            if let Some(process) = self.compute.must_upgrade().get_process(child.pid) {
                let inner = self.inner.clone();
                waits.push(async move {
                    let join = process.join().await;
                    let mut inner = inner.0.lock().unwrap();
                    inner.children.retain(|a| a.pid != child.pid);
                    (child, join)
                })
            }
        }
        let (child, res) = futures::future::select_all(waits.into_iter().map(|a| Box::pin(a)))
            .await
            .0;

        let code =
            res.unwrap_or_else(|e| e.as_exit_code().unwrap_or_else(|| Errno::Canceled.into()));

        Ok(Some((child.pid, code)))
    }

    /// Terminate the process and all its threads
    pub fn terminate(&self, exit_code: ExitCode) {
        // FIXME: this is wrong, threads might still be running!
        // Need special logic for the main thread.
        let guard = self.inner.0.lock().unwrap();
        for thread in guard.threads.values() {
            thread.set_status_finished(Ok(exit_code))
        }
    }

    // Releases the CPU backoff (if one is active)
    pub fn acquire_cpu_run_token(&self) -> CpuRunToken {
        self.cpu_run_tokens.fetch_add(1, Ordering::SeqCst);

        let mut inner = self.inner.0.lock().unwrap();
        for (_, waker) in inner.cpu_backoff_wakers.iter() {
            waker.wake_by_ref();
        }
        inner.cpu_backoff_wakers.clear();
        inner.cpu_backoff_time = Duration::ZERO;
        inner.cpu_run_cool_off = 0;

        CpuRunToken {
            tokens: self.cpu_run_tokens.clone(),
        }
    }

    // Determine if we should do a CPU backoff
    pub fn acquire_cpu_backoff_token(
        &self,
        tasks: &Arc<dyn VirtualTaskManager>,
    ) -> Option<CpuBackoffToken> {
        // If run tokens are held then we should allow executing to
        // continue at its top pace
        if self.cpu_run_tokens.load(Ordering::SeqCst) > 0 {
            return None;
        }

        let cpu_backoff_time = {
            let mut inner = self.inner.0.lock().unwrap();

            // check again as it might have changed (race condition)
            if self.cpu_run_tokens.load(Ordering::SeqCst) > 0 {
                return None;
            }

            // Check if a cool-off-period has passed
            let now =
                platform_clock_time_get(Snapshot0Clockid::Monotonic, 1_000_000).unwrap() as u128;
            if inner.cpu_run_cool_off == 0 {
                inner.cpu_run_cool_off =
                    now + (1_000_000 * inner.max_cpu_cool_off_time.as_millis());
            }
            if now <= inner.cpu_run_cool_off {
                return None;
            }

            // The amount of time we wait will be increased when a full
            // time slice is executed
            if inner.cpu_backoff_time == Duration::ZERO {
                inner.cpu_backoff_time = Duration::from_millis(1);
            }
            inner.cpu_backoff_time
        };
        let how_long = tasks.sleep_now(cpu_backoff_time);

        Some(CpuBackoffToken {
            cpu_backoff_time,
            wait: how_long,
            waker_id: None,
            inner: self.inner.clone(),
        })
    }
}

pub struct CpuRunToken {
    tokens: Arc<AtomicU32>,
}

impl Drop for CpuRunToken {
    fn drop(&mut self) {
        self.tokens.fetch_sub(1, Ordering::SeqCst);
    }
}

pub struct CpuBackoffToken {
    /// The amount of CPU backoff time we are currently waiting
    cpu_backoff_time: Duration,
    /// How long should the CPU backoff for
    wait: Pin<Box<dyn Future<Output = ()> + Send + Sync + 'static>>,
    /// ID used to unregister the wakers
    waker_id: Option<u64>,
    /// The inner protected region of the process with a conditional
    /// variable that is used for coordination such as checksums.
    inner: LockableWasiProcessInner,
}

impl Future for CpuBackoffToken {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let inner = self.inner.clone();
        let mut inner = inner.0.lock().unwrap();

        // Registering the waker will unregister any previous wakers
        // so that we don't go into an endless memory growth situation
        if let Some(waker_id) = self.waker_id.take() {
            if inner.cpu_backoff_wakers.remove(&waker_id).is_none() {
                // if we did not remove the waker, then someone else did
                // which means we were woken and should exit the backoff phase
                return Poll::Ready(());
            }
        }

        // Register ourselves as a waker to be woken
        let id = inner.cpu_backoff_waker_seed + 1;
        inner.cpu_backoff_waker_seed = id;
        inner.cpu_backoff_wakers.insert(id, cx.waker().clone());

        // Now poll the waiting period
        let ret = self.wait.poll_unpin(cx);

        // If we have reached the end of the wait period
        // then we need to exponentially grow it any future
        // backoff's so that it gets slower
        if ret.is_ready() {
            if self.cpu_backoff_time == inner.cpu_backoff_time {
                inner.cpu_backoff_time *= 2;
                if inner.cpu_backoff_time > inner.max_cpu_backoff_time {
                    inner.cpu_backoff_time = inner.max_cpu_backoff_time;
                }
            }
        }

        ret
    }
}

impl Drop for CpuBackoffToken {
    fn drop(&mut self) {
        if let Some(waker_id) = self.waker_id.take() {
            let mut inner = self.inner.0.lock().unwrap();
            inner.cpu_backoff_wakers.remove(&waker_id);
        }
    }
}

impl SignalHandlerAbi for WasiProcess {
    fn signal(&self, sig: u8) -> Result<(), SignalDeliveryError> {
        if let Ok(sig) = sig.try_into() {
            self.signal_process(sig);
            Ok(())
        } else {
            Err(SignalDeliveryError)
        }
    }
}
