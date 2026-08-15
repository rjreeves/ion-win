//! Execution-domain types and the authoritative in-memory execution registry.
//!
//! This module deliberately contains no process-launching code yet. It establishes
//! the lifecycle and ownership boundary described by `ARCHITECTURE.md` §44 so
//! existing launch paths can be migrated without changing their behavior.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex, OnceLock,
};
use std::time::{Duration, SystemTime};

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ExecutionId(u64);

impl ExecutionId {
    pub fn get(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ExecutionTarget {
    External { program: PathBuf, args: Vec<String> },
    Pipeline { display: String },
    Builtin { name: String, args: Vec<String> },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExecutionMode {
    Foreground,
    Background,
    Detached,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IoMode {
    Inherit,
    Redirected,
    Capture,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TerminalMode {
    Inherit,
    Redirected,
    PseudoConsole,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ExecutionLimits {
    pub memory_bytes: Option<u64>,
    pub process_count: Option<u32>,
}

/// Immutable launch intent. Fields are private so callers must construct a new
/// spec rather than mutating the intent of an execution already in flight.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExecutionSpec {
    target: ExecutionTarget,
    cwd: PathBuf,
    environment_overrides: BTreeMap<String, String>,
    mode: ExecutionMode,
    stdin: IoMode,
    stdout: IoMode,
    stderr: IoMode,
    terminal: TerminalMode,
    limits: ExecutionLimits,
    timeout: Option<Duration>,
}

impl ExecutionSpec {
    pub fn new(target: ExecutionTarget, cwd: PathBuf, mode: ExecutionMode) -> Self {
        Self {
            target,
            cwd,
            environment_overrides: BTreeMap::new(),
            mode,
            stdin: IoMode::Inherit,
            stdout: IoMode::Inherit,
            stderr: IoMode::Inherit,
            terminal: TerminalMode::Inherit,
            limits: ExecutionLimits::default(),
            timeout: None,
        }
    }

    pub fn target(&self) -> &ExecutionTarget {
        &self.target
    }
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }
    pub fn environment_overrides(&self) -> &BTreeMap<String, String> {
        &self.environment_overrides
    }
    pub fn mode(&self) -> ExecutionMode {
        self.mode
    }
    pub fn stdin(&self) -> IoMode {
        self.stdin
    }
    pub fn stdout(&self) -> IoMode {
        self.stdout
    }
    pub fn stderr(&self) -> IoMode {
        self.stderr
    }
    pub fn terminal(&self) -> TerminalMode {
        self.terminal
    }
    pub fn limits(&self) -> &ExecutionLimits {
        &self.limits
    }
    pub fn timeout(&self) -> Option<Duration> {
        self.timeout
    }

    pub fn with_environment_override(
        mut self,
        name: impl Into<String>,
        value: impl Into<String>,
    ) -> Self {
        self.environment_overrides.insert(name.into(), value.into());
        self
    }

    pub fn with_io(mut self, stdin: IoMode, stdout: IoMode, stderr: IoMode) -> Self {
        self.stdin = stdin;
        self.stdout = stdout;
        self.stderr = stderr;
        self
    }

    pub fn with_terminal(mut self, terminal: TerminalMode) -> Self {
        self.terminal = terminal;
        self
    }

    pub fn with_limits(mut self, limits: ExecutionLimits) -> Self {
        self.limits = limits;
        self
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }
}

#[derive(Clone, Default)]
pub struct CancellationToken(Arc<AtomicBool>);

impl CancellationToken {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

impl fmt::Debug for CancellationToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CancellationToken")
            .field("cancelled", &self.is_cancelled())
            .finish()
    }
}

#[derive(Clone, Default)]
pub struct SecretValue(String);

impl SecretValue {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SecretValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

/// Resolved, invocation-specific inputs. This value is never copied into
/// `ExecutionRecord`; in particular, temporary secrets remain live-only.
#[derive(Clone, Debug)]
pub struct ExecutionContext {
    effective_cwd: PathBuf,
    effective_environment: BTreeMap<String, String>,
    temporary_secrets: BTreeMap<String, SecretValue>,
    cancellation: CancellationToken,
    correlation_id: Option<String>,
}

impl ExecutionContext {
    pub fn new(effective_cwd: PathBuf, effective_environment: BTreeMap<String, String>) -> Self {
        Self {
            effective_cwd,
            effective_environment,
            temporary_secrets: BTreeMap::new(),
            cancellation: CancellationToken::default(),
            correlation_id: None,
        }
    }

    pub fn effective_cwd(&self) -> &Path {
        &self.effective_cwd
    }
    pub fn effective_environment(&self) -> &BTreeMap<String, String> {
        &self.effective_environment
    }
    pub fn temporary_secrets(&self) -> &BTreeMap<String, SecretValue> {
        &self.temporary_secrets
    }
    pub fn cancellation(&self) -> &CancellationToken {
        &self.cancellation
    }
    pub fn correlation_id(&self) -> Option<&str> {
        self.correlation_id.as_deref()
    }

    pub fn with_secret(mut self, name: impl Into<String>, value: SecretValue) -> Self {
        self.temporary_secrets.insert(name.into(), value);
        self
    }

    pub fn with_correlation_id(mut self, id: impl Into<String>) -> Self {
        self.correlation_id = Some(id.into());
        self
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExecutionState {
    Created,
    Starting,
    Running,
    Cancelling,
    Completed,
    Failed,
    Cancelled,
}

impl ExecutionState {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }

    fn can_transition_to(self, next: Self) -> bool {
        matches!(
            (self, next),
            (Self::Created, Self::Starting)
                | (Self::Starting, Self::Running | Self::Failed)
                | (
                    Self::Running,
                    Self::Completed | Self::Failed | Self::Cancelling
                )
                | (Self::Cancelling, Self::Cancelled)
        )
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExecutionResult {
    pub exit_code: Option<i32>,
    pub message: Option<String>,
}

impl ExecutionResult {
    pub fn new(exit_code: Option<i32>, message: Option<String>) -> Self {
        Self { exit_code, message }
    }
}

#[derive(Clone, Debug)]
pub struct Execution {
    id: ExecutionId,
    spec: ExecutionSpec,
    context: ExecutionContext,
    state: ExecutionState,
    created_at: SystemTime,
    started_at: Option<SystemTime>,
    finished_at: Option<SystemTime>,
    root_pid: Option<u32>,
    processes: Vec<ProcessRecord>,
    result: Option<ExecutionResult>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessRecord {
    pub pid: u32,
    pub exit_code: Option<i32>,
}

impl Execution {
    pub fn id(&self) -> ExecutionId {
        self.id
    }
    pub fn spec(&self) -> &ExecutionSpec {
        &self.spec
    }
    pub fn context(&self) -> &ExecutionContext {
        &self.context
    }
    pub fn state(&self) -> ExecutionState {
        self.state
    }
    pub fn created_at(&self) -> SystemTime {
        self.created_at
    }
    pub fn started_at(&self) -> Option<SystemTime> {
        self.started_at
    }
    pub fn finished_at(&self) -> Option<SystemTime> {
        self.finished_at
    }
    pub fn root_pid(&self) -> Option<u32> {
        self.root_pid
    }
    pub fn processes(&self) -> &[ProcessRecord] {
        &self.processes
    }
    pub fn result(&self) -> Option<&ExecutionResult> {
        self.result.as_ref()
    }

    pub fn set_root_pid(&mut self, pid: u32) {
        self.root_pid = Some(pid);
    }

    pub fn add_process(&mut self, pid: u32) {
        if self.root_pid.is_none() {
            self.root_pid = Some(pid);
        }
        self.processes.push(ProcessRecord {
            pid,
            exit_code: None,
        });
    }

    pub fn set_process_exit(&mut self, pid: u32, exit_code: Option<i32>) {
        if let Some(process) = self.processes.iter_mut().find(|process| process.pid == pid) {
            process.exit_code = exit_code;
        }
    }
}

#[derive(Clone, Debug)]
pub struct ExecutionRecord {
    pub id: ExecutionId,
    pub display: String,
    pub state: ExecutionState,
    pub created_at: SystemTime,
    pub started_at: Option<SystemTime>,
    pub finished_at: SystemTime,
    pub root_pid: Option<u32>,
    pub processes: Vec<ProcessRecord>,
    pub result: Option<ExecutionResult>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ExecutionError {
    IdExhausted,
    NotFound(ExecutionId),
    InvalidTransition {
        from: ExecutionState,
        to: ExecutionState,
    },
    TerminalResultRequired(ExecutionState),
    ResultBeforeTerminal(ExecutionState),
}

impl fmt::Display for ExecutionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::IdExhausted => f.write_str("execution ID space exhausted"),
            Self::NotFound(id) => write!(f, "execution {} was not found", id.get()),
            Self::InvalidTransition { from, to } => {
                write!(f, "invalid execution transition: {from:?} -> {to:?}")
            }
            Self::TerminalResultRequired(state) => write!(
                f,
                "a result is required when entering terminal state {state:?}"
            ),
            Self::ResultBeforeTerminal(state) => write!(
                f,
                "a result cannot be attached to non-terminal state {state:?}"
            ),
        }
    }
}

impl std::error::Error for ExecutionError {}

pub struct ExecutionManager {
    next_id: u64,
    active: HashMap<ExecutionId, Execution>,
    history: VecDeque<ExecutionRecord>,
    history_limit: usize,
}

const DEFAULT_HISTORY_LIMIT: usize = 1_000;
static MANAGER: OnceLock<Mutex<ExecutionManager>> = OnceLock::new();

fn global_manager() -> &'static Mutex<ExecutionManager> {
    MANAGER.get_or_init(|| Mutex::new(ExecutionManager::new(DEFAULT_HISTORY_LIMIT)))
}

fn manager_error(error: impl fmt::Display) -> std::io::Error {
    std::io::Error::other(format!("execution manager: {error}"))
}

fn with_manager<T>(
    action: impl FnOnce(&mut ExecutionManager) -> Result<T, ExecutionError>,
) -> std::io::Result<T> {
    let mut manager = global_manager()
        .lock()
        .map_err(|_| std::io::Error::other("execution manager lock was poisoned"))?;
    action(&mut manager).map_err(manager_error)
}

/// Launches and waits for one foreground external process through the
/// authoritative execution lifecycle. `jobctl` remains the Windows backend
/// for process-group creation and Ctrl+Break forwarding during this migration.
pub fn run_foreground_external(
    program: &Path,
    args: &[String],
) -> std::io::Result<std::process::ExitStatus> {
    let cwd = std::env::current_dir()?;
    let spec = ExecutionSpec::new(
        ExecutionTarget::External {
            program: program.to_path_buf(),
            args: args.to_vec(),
        },
        cwd.clone(),
        ExecutionMode::Foreground,
    );
    let context = ExecutionContext::new(cwd, std::env::vars().collect());
    let id = with_manager(|manager| manager.create(spec, context))?;
    with_manager(|manager| {
        manager.transition(id, ExecutionState::Starting, None)?;
        Ok(())
    })?;

    let child = match crate::jobctl::new_command(program).args(args).spawn() {
        Ok(child) => child,
        Err(error) => {
            let message = error.to_string();
            with_manager(|manager| {
                manager.transition(
                    id,
                    ExecutionState::Failed,
                    Some(ExecutionResult::new(None, Some(message))),
                )?;
                Ok(())
            })?;
            return Err(error);
        }
    };

    let pid = child.id();
    with_manager(|manager| {
        let execution = manager.get_mut(id).ok_or(ExecutionError::NotFound(id))?;
        execution.add_process(pid);
        manager.transition(id, ExecutionState::Running, None)?;
        Ok(())
    })?;

    let wait_result = crate::jobctl::wait_foreground(child);
    if let Ok(status) = &wait_result {
        with_manager(|manager| {
            let execution = manager.get_mut(id).ok_or(ExecutionError::NotFound(id))?;
            execution.set_process_exit(pid, status.code());
            Ok(())
        })?;
    }
    let (state, result) = match &wait_result {
        Ok(status) if status.success() => (
            ExecutionState::Completed,
            ExecutionResult::new(status.code(), None),
        ),
        Ok(status) => (
            ExecutionState::Failed,
            ExecutionResult::new(status.code(), Some("process exited unsuccessfully".into())),
        ),
        Err(error) => (
            ExecutionState::Failed,
            ExecutionResult::new(None, Some(format!("failed to wait for process: {error}"))),
        ),
    };
    with_manager(|manager| {
        manager.transition(id, state, Some(result))?;
        Ok(())
    })?;
    wait_result
}

/// Registers a foreground pipeline as one execution before any of its child
/// processes are launched. Individual PIDs are then attached as stages spawn.
pub fn begin_foreground_pipeline(display: String) -> std::io::Result<ExecutionId> {
    let cwd = std::env::current_dir()?;
    let spec = ExecutionSpec::new(
        ExecutionTarget::Pipeline { display },
        cwd.clone(),
        ExecutionMode::Foreground,
    );
    let context = ExecutionContext::new(cwd, std::env::vars().collect());
    let id = with_manager(|manager| manager.create(spec, context))?;
    with_manager(|manager| {
        manager.transition(id, ExecutionState::Starting, None)?;
        manager.transition(id, ExecutionState::Running, None)?;
        Ok(())
    })?;
    Ok(id)
}

pub fn register_pipeline_process(id: ExecutionId, pid: u32) -> std::io::Result<()> {
    with_manager(|manager| {
        let execution = manager.get_mut(id).ok_or(ExecutionError::NotFound(id))?;
        execution.add_process(pid);
        Ok(())
    })
}

pub fn fail_foreground_pipeline(id: ExecutionId, message: impl Into<String>) {
    let message = message.into();
    let _ = with_manager(|manager| {
        manager.transition(
            id,
            ExecutionState::Failed,
            Some(ExecutionResult::new(None, Some(message))),
        )?;
        Ok(())
    });
}

/// Takes ownership of every child handle in a foreground pipeline and waits
/// for all of them. The pipeline result remains the final stage's status,
/// matching the shell's established behavior.
pub fn wait_foreground_pipeline(
    id: ExecutionId,
    children: Vec<std::process::Child>,
) -> std::io::Result<bool> {
    let mut final_success = true;
    let mut final_code = None;
    let mut wait_error = None;

    for mut child in children {
        let pid = child.id();
        let waited = child.wait();
        crate::jobctl::unregister_foreground(pid);
        match waited {
            Ok(status) => {
                final_success = status.success();
                final_code = status.code();
                wait_error = None;
                with_manager(|manager| {
                    let execution = manager.get_mut(id).ok_or(ExecutionError::NotFound(id))?;
                    execution.set_process_exit(pid, status.code());
                    Ok(())
                })?;
            }
            Err(error) => {
                final_success = false;
                wait_error = Some(error);
            }
        }
    }

    let state = if final_success && wait_error.is_none() {
        ExecutionState::Completed
    } else {
        ExecutionState::Failed
    };
    let message = wait_error
        .as_ref()
        .map(|error| format!("failed to wait for pipeline process: {error}"));
    with_manager(|manager| {
        manager.transition(id, state, Some(ExecutionResult::new(final_code, message)))?;
        Ok(())
    })?;

    if let Some(error) = wait_error {
        Err(error)
    } else {
        Ok(final_success)
    }
}

impl ExecutionManager {
    pub fn new(history_limit: usize) -> Self {
        Self {
            next_id: 1,
            active: HashMap::new(),
            history: VecDeque::new(),
            history_limit,
        }
    }

    pub fn create(
        &mut self,
        spec: ExecutionSpec,
        context: ExecutionContext,
    ) -> Result<ExecutionId, ExecutionError> {
        let id = ExecutionId(self.next_id);
        self.next_id = self
            .next_id
            .checked_add(1)
            .ok_or(ExecutionError::IdExhausted)?;
        self.active.insert(
            id,
            Execution {
                id,
                spec,
                context,
                state: ExecutionState::Created,
                created_at: SystemTime::now(),
                started_at: None,
                finished_at: None,
                root_pid: None,
                processes: Vec::new(),
                result: None,
            },
        );
        Ok(id)
    }

    pub fn get(&self, id: ExecutionId) -> Option<&Execution> {
        self.active.get(&id)
    }
    pub fn get_mut(&mut self, id: ExecutionId) -> Option<&mut Execution> {
        self.active.get_mut(&id)
    }
    pub fn active(&self) -> impl Iterator<Item = &Execution> {
        self.active.values()
    }
    pub fn history(&self) -> impl Iterator<Item = &ExecutionRecord> {
        self.history.iter()
    }

    pub fn transition(
        &mut self,
        id: ExecutionId,
        next: ExecutionState,
        result: Option<ExecutionResult>,
    ) -> Result<Option<ExecutionRecord>, ExecutionError> {
        let execution = self
            .active
            .get_mut(&id)
            .ok_or(ExecutionError::NotFound(id))?;
        let from = execution.state;
        if !from.can_transition_to(next) {
            return Err(ExecutionError::InvalidTransition { from, to: next });
        }
        if next.is_terminal() && result.is_none() {
            return Err(ExecutionError::TerminalResultRequired(next));
        }
        if !next.is_terminal() && result.is_some() {
            return Err(ExecutionError::ResultBeforeTerminal(next));
        }

        let now = SystemTime::now();
        execution.state = next;
        if next == ExecutionState::Running {
            execution.started_at = Some(now);
        }
        if next.is_terminal() {
            execution.finished_at = Some(now);
            execution.result = result;
            let finished = self.active.remove(&id).expect("execution was just found");
            let record = Self::sanitize(finished);
            if self.history_limit > 0 {
                self.history.push_back(record.clone());
                while self.history.len() > self.history_limit {
                    self.history.pop_front();
                }
                return Ok(Some(record));
            }
            return Ok(None);
        }
        Ok(None)
    }

    fn sanitize(execution: Execution) -> ExecutionRecord {
        let display = match &execution.spec.target {
            ExecutionTarget::External { program, args } => {
                std::iter::once(program.to_string_lossy().into_owned())
                    .chain(args.iter().cloned())
                    .collect::<Vec<_>>()
                    .join(" ")
            }
            ExecutionTarget::Pipeline { display } => display.clone(),
            ExecutionTarget::Builtin { name, args } => std::iter::once(name.clone())
                .chain(args.iter().cloned())
                .collect::<Vec<_>>()
                .join(" "),
        };
        ExecutionRecord {
            id: execution.id,
            display,
            state: execution.state,
            created_at: execution.created_at,
            started_at: execution.started_at,
            finished_at: execution
                .finished_at
                .expect("terminal execution has a finish time"),
            root_pid: execution.root_pid,
            processes: execution.processes,
            result: execution.result,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (ExecutionSpec, ExecutionContext) {
        let cwd = PathBuf::from(r"C:\work");
        (
            ExecutionSpec::new(
                ExecutionTarget::External {
                    program: PathBuf::from("tool.exe"),
                    args: vec!["--run".into()],
                },
                cwd.clone(),
                ExecutionMode::Foreground,
            ),
            ExecutionContext::new(cwd, BTreeMap::new())
                .with_secret("token", SecretValue::new("never-persist-me")),
        )
    }

    #[test]
    fn successful_lifecycle_moves_execution_to_sanitized_history() {
        let mut manager = ExecutionManager::new(10);
        let (spec, context) = fixture();
        let id = manager.create(spec, context).unwrap();
        assert_eq!(manager.get(id).unwrap().state(), ExecutionState::Created);
        manager
            .transition(id, ExecutionState::Starting, None)
            .unwrap();
        manager
            .transition(id, ExecutionState::Running, None)
            .unwrap();
        manager.get_mut(id).unwrap().set_root_pid(42);
        let record = manager
            .transition(
                id,
                ExecutionState::Completed,
                Some(ExecutionResult::new(Some(0), None)),
            )
            .unwrap()
            .unwrap();
        assert_eq!(record.display, "tool.exe --run");
        assert_eq!(record.root_pid, Some(42));
        assert!(manager.get(id).is_none());
        assert_eq!(manager.history().count(), 1);
        assert!(!format!("{record:?}").contains("never-persist-me"));
    }

    #[test]
    fn launch_failure_is_reported_from_starting() {
        let mut manager = ExecutionManager::new(10);
        let (spec, context) = fixture();
        let id = manager.create(spec, context).unwrap();
        manager
            .transition(id, ExecutionState::Starting, None)
            .unwrap();
        let record = manager
            .transition(
                id,
                ExecutionState::Failed,
                Some(ExecutionResult::new(None, Some("spawn failed".into()))),
            )
            .unwrap()
            .unwrap();
        assert_eq!(record.state, ExecutionState::Failed);
        assert_eq!(
            record.result.as_ref().unwrap().message.as_deref(),
            Some("spawn failed")
        );
    }

    #[test]
    fn one_execution_tracks_multiple_pipeline_processes() {
        let mut manager = ExecutionManager::new(10);
        let cwd = PathBuf::from(r"C:\work");
        let spec = ExecutionSpec::new(
            ExecutionTarget::Pipeline {
                display: "producer | consumer".into(),
            },
            cwd.clone(),
            ExecutionMode::Foreground,
        );
        let id = manager
            .create(spec, ExecutionContext::new(cwd, BTreeMap::new()))
            .unwrap();
        manager
            .transition(id, ExecutionState::Starting, None)
            .unwrap();
        manager
            .transition(id, ExecutionState::Running, None)
            .unwrap();
        let execution = manager.get_mut(id).unwrap();
        execution.add_process(101);
        execution.add_process(202);
        execution.set_process_exit(101, Some(3));
        execution.set_process_exit(202, Some(0));

        let record = manager
            .transition(
                id,
                ExecutionState::Completed,
                Some(ExecutionResult::new(Some(0), None)),
            )
            .unwrap()
            .unwrap();
        assert_eq!(record.root_pid, Some(101));
        assert_eq!(
            record.processes,
            vec![
                ProcessRecord {
                    pid: 101,
                    exit_code: Some(3)
                },
                ProcessRecord {
                    pid: 202,
                    exit_code: Some(0)
                }
            ]
        );
        assert_eq!(record.state, ExecutionState::Completed);
    }

    #[test]
    fn cancellation_uses_validated_intermediate_state() {
        let mut manager = ExecutionManager::new(10);
        let (spec, context) = fixture();
        let id = manager.create(spec, context).unwrap();
        manager
            .transition(id, ExecutionState::Starting, None)
            .unwrap();
        manager
            .transition(id, ExecutionState::Running, None)
            .unwrap();
        manager
            .transition(id, ExecutionState::Cancelling, None)
            .unwrap();
        let record = manager
            .transition(
                id,
                ExecutionState::Cancelled,
                Some(ExecutionResult::new(None, Some("cancelled".into()))),
            )
            .unwrap()
            .unwrap();
        assert_eq!(record.state, ExecutionState::Cancelled);
    }

    #[test]
    fn invalid_transition_does_not_mutate_execution() {
        let mut manager = ExecutionManager::new(10);
        let (spec, context) = fixture();
        let id = manager.create(spec, context).unwrap();
        assert!(matches!(
            manager.transition(
                id,
                ExecutionState::Completed,
                Some(ExecutionResult::new(Some(0), None))
            ),
            Err(ExecutionError::InvalidTransition {
                from: ExecutionState::Created,
                to: ExecutionState::Completed
            })
        ));
        assert_eq!(manager.get(id).unwrap().state(), ExecutionState::Created);
    }

    #[test]
    fn history_is_bounded_and_zero_disables_retention() {
        let mut manager = ExecutionManager::new(1);
        for _ in 0..2 {
            let (spec, context) = fixture();
            let id = manager.create(spec, context).unwrap();
            manager
                .transition(id, ExecutionState::Starting, None)
                .unwrap();
            manager
                .transition(
                    id,
                    ExecutionState::Failed,
                    Some(ExecutionResult::new(None, Some("failed".into()))),
                )
                .unwrap();
        }
        assert_eq!(manager.history().count(), 1);
        assert_eq!(manager.history().next().unwrap().id.get(), 2);

        let mut manager = ExecutionManager::new(0);
        let (spec, context) = fixture();
        let id = manager.create(spec, context).unwrap();
        manager
            .transition(id, ExecutionState::Starting, None)
            .unwrap();
        assert!(manager
            .transition(
                id,
                ExecutionState::Failed,
                Some(ExecutionResult::new(None, None))
            )
            .unwrap()
            .is_none());
        assert_eq!(manager.history().count(), 0);
    }
}
