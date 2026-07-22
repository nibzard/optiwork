// ABOUTME: Generic, fail-closed orchestration for a frozen optimization campaign.
// ABOUTME: Campaign state, command output, and provenance are persisted as durable artifacts.

use std::collections::{BTreeMap, HashSet};
use std::env;
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, ExitStatus, Stdio};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[cfg(unix)]
use std::os::unix::process::CommandExt;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

const SPEC_VERSION: &str = "optiwork-campaign-v1";
const STATE_VERSION: &str = "optiwork-campaign-state-v1";
const MAX_PAIRED_BLOCKS: usize = 100_000;
const MAX_TIMEOUT_MS: u64 = 7 * 24 * 60 * 60 * 1_000;
const MAX_OUTPUT_BYTES: usize = 64 * 1024 * 1024;
const PAIRED_RUNS_PER_BLOCK: u64 = 4;
const MIN_PAIRED_MARGIN_MS: u64 = 100;
const USAGE: &str = "Usage: optikit-campaign --spec <campaign.json> --run-dir <new-dir>";

#[derive(Debug)]
struct Cli {
    spec: PathBuf,
    run_dir: PathBuf,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct CampaignSpec {
    version: String,
    id: String,
    paired: PathBuf,
    measure: String,
    environment: BTreeMap<String, String>,
    limits: LimitsSpec,
    max_candidates: usize,
    baseline: ArtifactSpec,
    candidates: Vec<CandidateSpec>,
    workloads: Vec<WorkloadSpec>,
    calibration: CalibrationSpec,
    exploration: DesignSpec,
    confirmation: DesignSpec,
    decision: DecisionSpec,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct LimitsSpec {
    gate_timeout_ms: u64,
    subject_timeout_ms: u64,
    paired_timeout_ms: u64,
    max_output_bytes: usize,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ArtifactSpec {
    id: String,
    binary: PathBuf,
    args: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct CandidateSpec {
    id: String,
    binary: PathBuf,
    args: Vec<String>,
    hypothesis: String,
}

impl CandidateSpec {
    fn as_artifact(&self) -> ArtifactSpec {
        ArtifactSpec {
            id: self.id.clone(),
            binary: self.binary.clone(),
            args: self.args.clone(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct WorkloadSpec {
    id: String,
    args: Vec<String>,
    gate_args: Vec<String>,
    #[serde(default)]
    artifacts: Vec<PathBuf>,
    count: u64,
    sessions: u64,
    calibration_blocks: usize,
    min_blocks: usize,
    max_blocks: usize,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct CalibrationSpec {
    order_seed: u64,
    seeds: Vec<u64>,
    target_speedup_percent: f64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct DesignSpec {
    order_seed: u64,
    seeds: Vec<u64>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct DecisionSpec {
    min_lower_bound_ratio: f64,
}

#[derive(Debug, Serialize)]
struct ProvenanceEntry {
    kind: String,
    id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    workload_id: Option<String>,
    path: String,
    sha256: String,
}

#[derive(Clone, Debug, Serialize)]
struct ResultSummary {
    workload_id: String,
    blocks: usize,
    lower_95_one_sided_ratio: f64,
    speedup_percent: f64,
    evidence: String,
}

#[derive(Clone, Debug, Serialize)]
struct CalibrationOutcome {
    workload_id: String,
    planned_blocks: usize,
    recommended_blocks: usize,
    selected_blocks: usize,
    log_ratio_sd: f64,
}

#[derive(Clone, Debug, Serialize)]
struct CandidateOutcome {
    candidate_id: String,
    hypothesis: String,
    baseline_before: String,
    baseline_after: String,
    decision: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    gate_failures: Vec<String>,
    workloads: Vec<ResultSummary>,
}

#[derive(Clone, Debug, Serialize)]
struct BaselineTransition {
    candidate_id: String,
    from: String,
    to: String,
}

#[derive(Clone, Debug, Serialize)]
struct ConfirmationOutcome {
    original_baseline: String,
    final_baseline: String,
    decision: String,
    workloads: Vec<ResultSummary>,
}

#[derive(Debug, Serialize)]
struct CampaignState {
    version: String,
    campaign_id: String,
    status: String,
    phase: String,
    last_event_sequence: u64,
    original_baseline: String,
    current_baseline: String,
    min_lower_bound_ratio: f64,
    calibrated_blocks: BTreeMap<String, usize>,
    calibrations: Vec<CalibrationOutcome>,
    candidates: Vec<CandidateOutcome>,
    promotions: Vec<BaselineTransition>,
    confirmation: Option<ConfirmationOutcome>,
    outcome: Option<String>,
    error: Option<String>,
}

#[derive(Serialize)]
struct Event<'a> {
    sequence: u64,
    timestamp: String,
    campaign_id: &'a str,
    phase: &'a str,
    #[serde(rename = "type")]
    kind: &'a str,
    payload: Value,
}

struct EventLog {
    file: File,
    campaign_id: String,
    sequence: u64,
}

impl EventLog {
    fn create(path: &Path, campaign_id: &str) -> Result<Self, String> {
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .map_err(|error| format!("could not create {}: {error}", path.display()))?;
        Ok(Self {
            file,
            campaign_id: campaign_id.to_owned(),
            sequence: 0,
        })
    }

    fn append(&mut self, phase: &str, kind: &str, payload: Value) -> Result<u64, String> {
        self.sequence = self
            .sequence
            .checked_add(1)
            .ok_or_else(|| "event sequence overflowed".to_owned())?;
        let event = Event {
            sequence: self.sequence,
            timestamp: timestamp(),
            campaign_id: &self.campaign_id,
            phase,
            kind,
            payload,
        };
        serde_json::to_writer(&mut self.file, &event)
            .map_err(|error| format!("could not encode event: {error}"))?;
        self.file
            .write_all(b"\n")
            .map_err(|error| format!("could not terminate event: {error}"))?;
        self.file
            .sync_data()
            .map_err(|error| format!("could not sync event log: {error}"))?;
        Ok(self.sequence)
    }
}

struct CapturedOutput {
    success: bool,
    status: String,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    stdout_path: String,
    stderr_path: String,
    operational_error: Option<String>,
}

struct ProcessCapture {
    success: bool,
    status: String,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    operational_error: Option<String>,
}

#[derive(Default)]
struct StreamCapture {
    bytes: Vec<u8>,
    exceeded: bool,
    read_error: Option<String>,
    finished: bool,
}

struct StreamSnapshot {
    bytes: Vec<u8>,
    exceeded: bool,
}

struct StreamProgress {
    exceeded: bool,
    read_error: Option<String>,
    finished: bool,
}

enum CaptureEnd {
    Completed(ExitStatus),
    Timeout,
    OutputOverflow,
    ReadError(String),
    WaitError(String),
}

fn stream_lock(stream: &Mutex<StreamCapture>) -> MutexGuard<'_, StreamCapture> {
    stream
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn take_stream_snapshot(stream: &Arc<Mutex<StreamCapture>>) -> StreamSnapshot {
    let mut stream = stream_lock(stream);
    StreamSnapshot {
        bytes: std::mem::take(&mut stream.bytes),
        exceeded: stream.exceeded,
    }
}

fn stream_progress(stream: &Arc<Mutex<StreamCapture>>) -> StreamProgress {
    let stream = stream_lock(stream);
    StreamProgress {
        exceeded: stream.exceeded,
        read_error: stream.read_error.clone(),
        finished: stream.finished,
    }
}

fn spawn_stream_reader<R>(
    name: &str,
    mut reader: R,
    stream: Arc<Mutex<StreamCapture>>,
    max_output_bytes: usize,
) -> io::Result<thread::JoinHandle<()>>
where
    R: Read + Send + 'static,
{
    thread::Builder::new().name(name.to_owned()).spawn(move || {
        let mut buffer = [0_u8; 8 * 1024];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => {
                    stream_lock(&stream).finished = true;
                    break;
                }
                Ok(read) => {
                    let mut stream = stream_lock(&stream);
                    let retained = read.min(max_output_bytes.saturating_sub(stream.bytes.len()));
                    stream.bytes.extend_from_slice(&buffer[..retained]);
                    stream.exceeded |= retained < read;
                }
                Err(error) => {
                    let mut stream = stream_lock(&stream);
                    stream.read_error = Some(error.to_string());
                    stream.finished = true;
                    break;
                }
            }
        }
    })
}

#[cfg(unix)]
fn isolate_command_process(command: &mut Command) {
    command.process_group(0);
}

#[cfg(not(unix))]
fn isolate_command_process(_command: &mut Command) {}

#[cfg(unix)]
fn kill_command_process_tree(child: &mut Child) -> io::Result<()> {
    let process_group = libc::pid_t::try_from(child.id()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "command process ID does not fit pid_t",
        )
    })?;
    // SAFETY: the positive child ID is also the process-group ID established
    // immediately before spawn. A negative PID addresses every process in it.
    let result = unsafe { libc::kill(-process_group, libc::SIGKILL) };
    if result == 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        Ok(())
    } else {
        Err(error)
    }
}

#[cfg(unix)]
fn terminate_and_reap_command(child: &mut Child, direct_child_reaped: bool) -> Result<(), String> {
    let mut errors = Vec::new();
    if let Err(error) = kill_command_process_tree(child) {
        errors.push(format!(
            "could not terminate command process group: {error}"
        ));
        if !direct_child_reaped {
            let _ = child.kill();
        }
    }
    if !direct_child_reaped {
        if let Err(error) = child.wait() {
            errors.push(format!("could not reap direct command child: {error}"));
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

#[cfg(not(unix))]
fn terminate_and_reap_command(child: &mut Child, direct_child_reaped: bool) -> Result<(), String> {
    if direct_child_reaped {
        return Ok(());
    }
    let mut errors = Vec::new();
    if let Err(error) = child.kill() {
        errors.push(format!("could not terminate direct command child: {error}"));
    }
    if let Err(error) = child.wait() {
        errors.push(format!("could not reap direct command child: {error}"));
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

fn join_readers(readers: Vec<(&'static str, thread::JoinHandle<()>)>) -> Vec<String> {
    readers
        .into_iter()
        .filter_map(|(stream, reader)| {
            reader
                .join()
                .err()
                .map(|_| format!("{stream} reader panicked"))
        })
        .collect()
}

fn operational_process_capture(
    status: &str,
    error: String,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
) -> ProcessCapture {
    ProcessCapture {
        success: false,
        status: status.to_owned(),
        stdout,
        stderr,
        operational_error: Some(error),
    }
}

fn capture_process(
    command: &mut Command,
    program: &Path,
    timeout_ms: u64,
    max_output_bytes: usize,
) -> ProcessCapture {
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    isolate_command_process(command);
    let deadline = match Instant::now().checked_add(Duration::from_millis(timeout_ms)) {
        Some(deadline) => deadline,
        None => {
            return operational_process_capture(
                "wait_error",
                "timeout is too large for this platform".to_owned(),
                Vec::new(),
                Vec::new(),
            );
        }
    };
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            return operational_process_capture(
                "spawn_error",
                format!("could not spawn {}: {error}", program.display()),
                Vec::new(),
                Vec::new(),
            );
        }
    };
    let child_stdout = match child.stdout.take() {
        Some(stdout) => stdout,
        None => {
            let cleanup = terminate_and_reap_command(&mut child, false)
                .err()
                .map(|error| format!("; cleanup failed: {error}"))
                .unwrap_or_default();
            return operational_process_capture(
                "stdout_read_error",
                format!(
                    "could not capture stdout from {}{cleanup}",
                    program.display()
                ),
                Vec::new(),
                Vec::new(),
            );
        }
    };
    let child_stderr = match child.stderr.take() {
        Some(stderr) => stderr,
        None => {
            drop(child_stdout);
            let cleanup = terminate_and_reap_command(&mut child, false)
                .err()
                .map(|error| format!("; cleanup failed: {error}"))
                .unwrap_or_default();
            return operational_process_capture(
                "stderr_read_error",
                format!(
                    "could not capture stderr from {}{cleanup}",
                    program.display()
                ),
                Vec::new(),
                Vec::new(),
            );
        }
    };

    let stdout_capture = Arc::new(Mutex::new(StreamCapture::default()));
    let stderr_capture = Arc::new(Mutex::new(StreamCapture::default()));
    let stdout_reader = match spawn_stream_reader(
        "optikit-campaign-stdout",
        child_stdout,
        Arc::clone(&stdout_capture),
        max_output_bytes,
    ) {
        Ok(reader) => reader,
        Err(error) => {
            drop(child_stderr);
            let cleanup = terminate_and_reap_command(&mut child, false)
                .err()
                .map(|cleanup| format!("; cleanup failed: {cleanup}"))
                .unwrap_or_default();
            return operational_process_capture(
                "stdout_read_error",
                format!(
                    "could not start stdout reader for {}: {error}{cleanup}",
                    program.display()
                ),
                Vec::new(),
                Vec::new(),
            );
        }
    };
    let stderr_reader = match spawn_stream_reader(
        "optikit-campaign-stderr",
        child_stderr,
        Arc::clone(&stderr_capture),
        max_output_bytes,
    ) {
        Ok(reader) => reader,
        Err(error) => {
            let cleanup = terminate_and_reap_command(&mut child, false);
            let safe_to_join = cfg!(unix) && cleanup.is_ok();
            let reader_errors = if safe_to_join {
                join_readers(vec![("stdout", stdout_reader)])
            } else {
                drop(stdout_reader);
                Vec::new()
            };
            let stdout = take_stream_snapshot(&stdout_capture).bytes;
            let mut detail = format!(
                "could not start stderr reader for {}: {error}",
                program.display()
            );
            if let Err(cleanup) = cleanup {
                detail.push_str(&format!("; cleanup failed: {cleanup}"));
            }
            for reader_error in reader_errors {
                detail.push_str(&format!("; {reader_error}"));
            }
            return operational_process_capture("stderr_read_error", detail, stdout, Vec::new());
        }
    };

    let mut child_status = None;
    let end = loop {
        if child_status.is_none() {
            match child.try_wait() {
                Ok(status) => child_status = status,
                Err(error) => break CaptureEnd::WaitError(error.to_string()),
            }
        }
        let stdout = stream_progress(&stdout_capture);
        let stderr = stream_progress(&stderr_capture);
        if let Some(error) = stdout.read_error {
            break CaptureEnd::ReadError(format!("stdout: {error}"));
        }
        if let Some(error) = stderr.read_error {
            break CaptureEnd::ReadError(format!("stderr: {error}"));
        }
        if stdout.exceeded || stderr.exceeded {
            break CaptureEnd::OutputOverflow;
        }
        if stdout.finished && stderr.finished {
            if let Some(status) = child_status.take() {
                break CaptureEnd::Completed(status);
            }
        }
        let now = Instant::now();
        if now >= deadline {
            break CaptureEnd::Timeout;
        }
        thread::sleep(Duration::from_millis(5).min(deadline.saturating_duration_since(now)));
    };

    if let CaptureEnd::Completed(status) = end {
        let reader_errors =
            join_readers(vec![("stdout", stdout_reader), ("stderr", stderr_reader)]);
        let stdout = take_stream_snapshot(&stdout_capture);
        let stderr = take_stream_snapshot(&stderr_capture);
        if !reader_errors.is_empty() {
            return operational_process_capture(
                "read_error",
                reader_errors.join("; "),
                stdout.bytes,
                stderr.bytes,
            );
        }
        return ProcessCapture {
            success: status.success(),
            status: status
                .code()
                .map(|code| code.to_string())
                .unwrap_or_else(|| "terminated_by_signal".to_owned()),
            stdout: stdout.bytes,
            stderr: stderr.bytes,
            operational_error: None,
        };
    }

    let cleanup = terminate_and_reap_command(&mut child, child_status.is_some());
    let safe_to_join = cfg!(unix) && cleanup.is_ok();
    let reader_errors = if safe_to_join {
        join_readers(vec![("stdout", stdout_reader), ("stderr", stderr_reader)])
    } else {
        drop(stdout_reader);
        drop(stderr_reader);
        Vec::new()
    };
    let stdout = take_stream_snapshot(&stdout_capture);
    let stderr = take_stream_snapshot(&stderr_capture);
    let (status, mut detail) = match end {
        CaptureEnd::Completed(_) => unreachable!("completed capture returned above"),
        CaptureEnd::Timeout => (
            "timeout",
            format!("{} timed out after {timeout_ms} ms", program.display()),
        ),
        CaptureEnd::OutputOverflow => {
            let streams = match (stdout.exceeded, stderr.exceeded) {
                (true, true) => "stdout and stderr",
                (true, false) => "stdout",
                (false, true) => "stderr",
                (false, false) => "output",
            };
            (
                "output_overflow",
                format!(
                    "{} {streams} exceeded the per-stream limit of {max_output_bytes} bytes",
                    program.display()
                ),
            )
        }
        CaptureEnd::ReadError(error) => (
            "read_error",
            format!("could not read output from {}: {error}", program.display()),
        ),
        CaptureEnd::WaitError(error) => (
            "wait_error",
            format!("could not wait for {}: {error}", program.display()),
        ),
    };
    if let Err(error) = cleanup {
        detail.push_str(&format!("; cleanup failed: {error}"));
    }
    for error in reader_errors {
        detail.push_str(&format!("; {error}"));
    }
    operational_process_capture(status, detail, stdout.bytes, stderr.bytes)
}

struct RunContext {
    run_dir: PathBuf,
    raw_dir: PathBuf,
    log: EventLog,
    state: CampaignState,
    command_sequence: u64,
    expected_file_hashes: BTreeMap<PathBuf, String>,
    child_environment: BTreeMap<String, String>,
    max_output_bytes: usize,
}

impl RunContext {
    fn create(
        run_dir: &Path,
        spec: &CampaignSpec,
        spec_bytes: &[u8],
        spec_sha256: &str,
        provenance: &[ProvenanceEntry],
    ) -> Result<Self, String> {
        if let Some(parent) = run_dir
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent).map_err(|error| {
                format!(
                    "could not create run-directory parent {}: {error}",
                    parent.display()
                )
            })?;
        }
        fs::create_dir(run_dir).map_err(|error| {
            format!(
                "could not create run directory {}: {error}",
                run_dir.display()
            )
        })?;
        let raw_dir = run_dir.join("raw");
        fs::create_dir(&raw_dir)
            .map_err(|error| format!("could not create {}: {error}", raw_dir.display()))?;
        write_new_sync(&run_dir.join("spec.json"), spec_bytes)?;
        let mut provenance_bytes = serde_json::to_vec_pretty(&json!({
            "version": "optiwork-provenance-v1",
            "campaign_id": spec.id,
            "spec_sha256": spec_sha256,
            "host": {
                "recorded_at": timestamp(),
                "working_directory": env::current_dir().ok().map(|path| path.display().to_string()),
                "os": env::consts::OS,
                "architecture": env::consts::ARCH,
                "available_parallelism": std::thread::available_parallelism().ok().map(std::num::NonZeroUsize::get),
            },
            "child_environment": spec.environment,
            "entries": provenance,
        }))
        .map_err(|error| format!("could not encode provenance: {error}"))?;
        provenance_bytes.push(b'\n');
        write_new_sync(&run_dir.join("provenance.json"), &provenance_bytes)?;
        sync_directory(run_dir)?;

        let log = EventLog::create(&run_dir.join("events.jsonl"), &spec.id)?;
        let state = CampaignState {
            version: STATE_VERSION.to_owned(),
            campaign_id: spec.id.clone(),
            status: "running".to_owned(),
            phase: "initialized".to_owned(),
            last_event_sequence: 0,
            original_baseline: spec.baseline.id.clone(),
            current_baseline: spec.baseline.id.clone(),
            min_lower_bound_ratio: spec.decision.min_lower_bound_ratio,
            calibrated_blocks: BTreeMap::new(),
            calibrations: Vec::new(),
            candidates: Vec::new(),
            promotions: Vec::new(),
            confirmation: None,
            outcome: None,
            error: None,
        };
        let mut context = Self {
            run_dir: run_dir.to_path_buf(),
            raw_dir,
            log,
            state,
            command_sequence: 0,
            expected_file_hashes: provenance
                .iter()
                .map(|entry| (PathBuf::from(&entry.path), entry.sha256.clone()))
                .collect(),
            child_environment: spec.environment.clone(),
            max_output_bytes: spec.limits.max_output_bytes,
        };
        context.emit(
            "initialized",
            "campaign_started",
            json!({
                "spec_sha256": spec_sha256,
                "spec_copy": "spec.json",
                "measure": spec.measure,
                "limits": spec.limits,
                "child_environment": spec.environment,
                "max_candidates": spec.max_candidates,
                "candidate_order": spec.candidates.iter().map(|candidate| &candidate.id).collect::<Vec<_>>(),
                "workload_order": spec.workloads.iter().map(|workload| &workload.id).collect::<Vec<_>>(),
                "provenance": provenance,
            }),
        )?;
        context.checkpoint()?;
        Ok(context)
    }

    fn emit(&mut self, phase: &str, kind: &str, payload: Value) -> Result<(), String> {
        let sequence = self.log.append(phase, kind, payload)?;
        self.state.last_event_sequence = sequence;
        Ok(())
    }

    fn set_phase(&mut self, phase: &str, payload: Value) -> Result<(), String> {
        self.state.phase = phase.to_owned();
        self.emit(phase, "phase_started", payload)?;
        self.checkpoint()
    }

    fn checkpoint(&self) -> Result<(), String> {
        let mut bytes = serde_json::to_vec_pretty(&self.state)
            .map_err(|error| format!("could not encode campaign state: {error}"))?;
        bytes.push(b'\n');
        write_atomic(&self.run_dir, "state.json", &bytes)
    }

    fn capture(
        &mut self,
        phase: &str,
        command_kind: &str,
        label: &str,
        program: &Path,
        args: &[String],
        timeout_ms: u64,
    ) -> Result<CapturedOutput, String> {
        self.verify_file(phase, program, command_kind)?;
        self.command_sequence = self
            .command_sequence
            .checked_add(1)
            .ok_or_else(|| "command sequence overflowed".to_owned())?;
        let stem = format!(
            "{:04}-{}-{}",
            self.command_sequence,
            command_kind,
            safe_filename_component(label)
        );
        let stdout_name = format!("raw/{stem}.stdout");
        let stderr_name = format!("raw/{stem}.stderr");
        let argv = std::iter::once(program.display().to_string())
            .chain(args.iter().cloned())
            .collect::<Vec<_>>();
        self.emit(
            phase,
            "command_started",
            json!({
                "command_sequence": self.command_sequence,
                "command_kind": command_kind,
                "label": label,
                "argv": argv,
                "stdout_path": stdout_name,
                "stderr_path": stderr_name,
                "timeout_ms": timeout_ms,
                "max_output_bytes_per_stream": self.max_output_bytes,
            }),
        )?;

        let mut command = Command::new(program);
        command
            .args(args.iter().map(OsString::from))
            .env_clear()
            .envs(&self.child_environment);
        let process = capture_process(&mut command, program, timeout_ms, self.max_output_bytes);
        let ProcessCapture {
            success,
            status,
            stdout,
            stderr,
            operational_error,
        } = process;
        write_new_sync(&self.raw_dir.join(format!("{stem}.stdout")), &stdout)?;
        write_new_sync(&self.raw_dir.join(format!("{stem}.stderr")), &stderr)?;
        sync_directory(&self.raw_dir)?;
        let stdout_text = String::from_utf8_lossy(&stdout).into_owned();
        let stderr_text = String::from_utf8_lossy(&stderr).into_owned();
        self.emit(
            phase,
            "command_completed",
            json!({
                "command_sequence": self.command_sequence,
                "command_kind": command_kind,
                "label": label,
                "argv": argv,
                "success": success,
                "status": status,
                "stdout_path": stdout_name,
                "stderr_path": stderr_name,
                "stdout_sha256": sha256_bytes(&stdout),
                "stderr_sha256": sha256_bytes(&stderr),
                "stdout_utf8": std::str::from_utf8(&stdout).is_ok(),
                "stderr_utf8": std::str::from_utf8(&stderr).is_ok(),
                "operational_error": operational_error.as_deref(),
                "stdout": stdout_text,
                "stderr": stderr_text,
            }),
        )?;
        Ok(CapturedOutput {
            success,
            status,
            stdout,
            stderr,
            stdout_path: stdout_name,
            stderr_path: stderr_name,
            operational_error,
        })
    }

    fn verify_file(&mut self, phase: &str, path: &Path, purpose: &str) -> Result<(), String> {
        let expected = self
            .expected_file_hashes
            .get(path)
            .cloned()
            .ok_or_else(|| {
                format!(
                    "{purpose} {} was not included in frozen provenance",
                    path.display()
                )
            })?;
        let actual = sha256_file(path)?;
        let matches = actual == expected;
        self.emit(
            phase,
            "artifact_identity_checked",
            json!({
                "purpose": purpose,
                "path": path.display().to_string(),
                "expected_sha256": expected,
                "actual_sha256": actual,
                "matches": matches,
            }),
        )?;
        if matches {
            Ok(())
        } else {
            Err(format!(
                "{purpose} {} changed after campaign initialization",
                path.display()
            ))
        }
    }

    fn verify_workload_artifacts(
        &mut self,
        phase: &str,
        workload: &WorkloadSpec,
    ) -> Result<(), String> {
        for artifact in &workload.artifacts {
            self.verify_file(phase, artifact, "workload_artifact")?;
        }
        Ok(())
    }

    fn verify_all_provenance(&mut self, phase: &str) -> Result<(), String> {
        let files = self
            .expected_file_hashes
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        for file in files {
            self.verify_file(phase, &file, "final_provenance")?;
        }
        Ok(())
    }

    fn complete(&mut self, outcome: &str) -> Result<(), String> {
        self.state.status = "completed".to_owned();
        self.state.phase = "complete".to_owned();
        self.state.outcome = Some(outcome.to_owned());
        self.emit(
            "complete",
            "campaign_completed",
            json!({
                "outcome": outcome,
                "original_baseline": self.state.original_baseline,
                "final_baseline": self.state.current_baseline,
                "promotions": self.state.promotions.len(),
            }),
        )?;
        self.checkpoint()?;
        self.write_report()
    }

    fn abort(&mut self, error: &str) -> Result<(), String> {
        self.state.status = "failed".to_owned();
        self.state.phase = "failed".to_owned();
        self.state.outcome = Some("operational_failure".to_owned());
        self.state.error = Some(error.to_owned());
        let event_result = self.emit("failed", "campaign_aborted", json!({"error": error}));
        let state_result = self.checkpoint();
        let report_result = self.write_report();
        event_result.and(state_result).and(report_result)
    }

    fn write_report(&self) -> Result<(), String> {
        let report = render_report(&self.state);
        write_atomic(&self.run_dir, "report.md", report.as_bytes())
    }
}

fn parse_args(mut args: impl Iterator<Item = String>) -> Result<Option<Cli>, String> {
    let mut spec = None;
    let mut run_dir = None;
    while let Some(argument) = args.next() {
        if matches!(argument.as_str(), "-h" | "--help") {
            return Ok(None);
        }
        let value = |args: &mut dyn Iterator<Item = String>, option: &str| {
            args.next()
                .ok_or_else(|| format!("missing value after `{option}`"))
        };
        match argument.as_str() {
            "--spec" => {
                if spec.is_some() {
                    return Err("`--spec` specified more than once".to_owned());
                }
                spec = Some(PathBuf::from(value(&mut args, "--spec")?));
            }
            "--run-dir" => {
                if run_dir.is_some() {
                    return Err("`--run-dir` specified more than once".to_owned());
                }
                run_dir = Some(PathBuf::from(value(&mut args, "--run-dir")?));
            }
            _ => return Err(format!("unknown option `{argument}`")),
        }
    }
    Ok(Some(Cli {
        spec: spec.ok_or_else(|| "missing required `--spec`".to_owned())?,
        run_dir: run_dir.ok_or_else(|| "missing required `--run-dir`".to_owned())?,
    }))
}

fn parse_spec(path: &Path) -> Result<(CampaignSpec, Vec<u8>, PathBuf), String> {
    let canonical_path = fs::canonicalize(path)
        .map_err(|error| format!("could not resolve spec {}: {error}", path.display()))?;
    let bytes = fs::read(&canonical_path)
        .map_err(|error| format!("could not read spec {}: {error}", canonical_path.display()))?;
    let spec = serde_json::from_slice(&bytes).map_err(|error| {
        format!(
            "invalid campaign spec {}: {error}",
            canonical_path.display()
        )
    })?;
    Ok((spec, bytes, canonical_path))
}

fn validate(spec: &mut CampaignSpec, run_dir: &Path) -> Result<(), String> {
    if spec.version != SPEC_VERSION {
        return Err(format!(
            "unsupported campaign version `{}`; expected `{SPEC_VERSION}`",
            spec.version
        ));
    }
    validate_id(&spec.id, "campaign")?;
    if spec.measure.is_empty()
        || spec
            .measure
            .bytes()
            .any(|byte| byte.is_ascii_whitespace() || matches!(byte, b'=' | 0))
    {
        return Err(
            "measure must be a non-empty result token without whitespace, '=' or NUL".to_owned(),
        );
    }
    validate_timeout("gate_timeout_ms", spec.limits.gate_timeout_ms)?;
    validate_timeout("subject_timeout_ms", spec.limits.subject_timeout_ms)?;
    validate_timeout("paired_timeout_ms", spec.limits.paired_timeout_ms)?;
    if spec.limits.max_output_bytes == 0 || spec.limits.max_output_bytes > MAX_OUTPUT_BYTES {
        return Err(format!(
            "max_output_bytes must be between 1 and {MAX_OUTPUT_BYTES}"
        ));
    }
    for (key, value) in &spec.environment {
        if key.is_empty() || key.contains(['=', '\0']) {
            return Err(
                "environment keys must be non-empty and contain neither '=' nor NUL".to_owned(),
            );
        }
        if value.contains('\0') {
            return Err(format!(
                "environment value for `{key}` must not contain a NUL byte"
            ));
        }
    }
    if spec.max_candidates == 0 {
        return Err("max_candidates must be positive".to_owned());
    }
    if spec.candidates.is_empty() {
        return Err("candidates must not be empty".to_owned());
    }
    if spec.candidates.len() > spec.max_candidates {
        return Err(format!(
            "candidate count {} exceeds max_candidates {}",
            spec.candidates.len(),
            spec.max_candidates
        ));
    }
    if spec.workloads.is_empty() {
        return Err("workloads must not be empty".to_owned());
    }
    if spec.calibration.seeds.is_empty() {
        return Err("calibration seeds must not be empty".to_owned());
    }
    if spec.exploration.seeds.is_empty() {
        return Err("exploration seeds must not be empty".to_owned());
    }
    if spec.confirmation.seeds.is_empty() {
        return Err("confirmation seeds must not be empty".to_owned());
    }
    if !spec.calibration.target_speedup_percent.is_finite()
        || spec.calibration.target_speedup_percent <= 0.0
    {
        return Err("target_speedup_percent must be finite and positive".to_owned());
    }
    if !spec.decision.min_lower_bound_ratio.is_finite() || spec.decision.min_lower_bound_ratio < 1.0
    {
        return Err("min_lower_bound_ratio must be finite and at least 1.0".to_owned());
    }
    let order_seeds = [
        spec.calibration.order_seed,
        spec.exploration.order_seed,
        spec.confirmation.order_seed,
    ];
    if order_seeds.iter().copied().collect::<HashSet<_>>().len() != order_seeds.len() {
        return Err(
            "calibration, exploration, and confirmation order seeds must differ".to_owned(),
        );
    }
    let exploration_seeds = spec
        .exploration
        .seeds
        .iter()
        .copied()
        .collect::<HashSet<_>>();
    if spec
        .confirmation
        .seeds
        .iter()
        .any(|seed| exploration_seeds.contains(seed))
    {
        return Err("exploration and confirmation seeds must be disjoint".to_owned());
    }

    ensure_absent(run_dir, "run directory")?;
    let working_dir = env::current_dir()
        .map_err(|error| format!("could not determine working directory: {error}"))?;
    resolve_file(&mut spec.paired, &working_dir, "paired runner")?;

    let mut artifact_ids = HashSet::new();
    validate_id(&spec.baseline.id, "baseline")?;
    if !artifact_ids.insert(spec.baseline.id.clone()) {
        return Err(format!("duplicate artifact id `{}`", spec.baseline.id));
    }
    validate_args(
        &spec.baseline.args,
        &format!("baseline `{}`", spec.baseline.id),
    )?;
    resolve_file(
        &mut spec.baseline.binary,
        &working_dir,
        &format!("baseline binary `{}`", spec.baseline.id),
    )?;

    for candidate in &mut spec.candidates {
        validate_id(&candidate.id, "candidate")?;
        if !artifact_ids.insert(candidate.id.clone()) {
            return Err(format!("duplicate artifact id `{}`", candidate.id));
        }
        if candidate.hypothesis.trim().is_empty() || candidate.hypothesis.contains('\0') {
            return Err(format!(
                "candidate `{}` hypothesis must be non-empty and contain no NUL byte",
                candidate.id
            ));
        }
        validate_args(&candidate.args, &format!("candidate `{}`", candidate.id))?;
        resolve_file(
            &mut candidate.binary,
            &working_dir,
            &format!("candidate binary `{}`", candidate.id),
        )?;
    }

    let mut workload_ids = HashSet::new();
    for workload in &mut spec.workloads {
        validate_id(&workload.id, "workload")?;
        if !workload_ids.insert(workload.id.clone()) {
            return Err(format!("duplicate workload id `{}`", workload.id));
        }
        validate_args(&workload.args, &format!("workload `{}`", workload.id))?;
        validate_args(
            &workload.gate_args,
            &format!("workload `{}` gate", workload.id),
        )?;
        if workload.count == 0 || workload.sessions == 0 {
            return Err(format!(
                "workload `{}` count and sessions must be positive",
                workload.id
            ));
        }
        workload
            .count
            .checked_mul(workload.sessions)
            .ok_or_else(|| format!("workload `{}` count times sessions overflows", workload.id))?;
        if workload.calibration_blocks < 2 {
            return Err(format!(
                "workload `{}` calibration_blocks must be at least 2",
                workload.id
            ));
        }
        if workload.calibration_blocks > MAX_PAIRED_BLOCKS {
            return Err(format!(
                "workload `{}` calibration_blocks must not exceed {MAX_PAIRED_BLOCKS}",
                workload.id
            ));
        }
        if workload.min_blocks < 2 {
            return Err(format!(
                "workload `{}` min_blocks must be at least 2",
                workload.id
            ));
        }
        if workload.max_blocks < workload.min_blocks {
            return Err(format!(
                "workload `{}` max_blocks must be at least min_blocks",
                workload.id
            ));
        }
        if workload.max_blocks > MAX_PAIRED_BLOCKS {
            return Err(format!(
                "workload `{}` max_blocks must not exceed {MAX_PAIRED_BLOCKS}",
                workload.id
            ));
        }
        for (index, artifact) in workload.artifacts.iter_mut().enumerate() {
            resolve_file(
                artifact,
                &working_dir,
                &format!("workload `{}` artifact {index}", workload.id),
            )?;
        }
    }
    let largest_paired_blocks = spec
        .workloads
        .iter()
        .map(|workload| workload.calibration_blocks.max(workload.max_blocks))
        .max()
        .ok_or_else(|| "workloads must not be empty".to_owned())?;
    let largest_paired_blocks = u64::try_from(largest_paired_blocks)
        .map_err(|_| "largest paired block count is too large for timeout arithmetic".to_owned())?;
    let planned_subject_runs = largest_paired_blocks
        .checked_mul(PAIRED_RUNS_PER_BLOCK)
        .ok_or_else(|| "paired subject-run count overflows".to_owned())?;
    let subject_budget_ms = planned_subject_runs
        .checked_mul(spec.limits.subject_timeout_ms)
        .ok_or_else(|| "worst-case paired subject timeout budget overflows".to_owned())?;
    let scheduling_margin_ms = (subject_budget_ms / 10).max(MIN_PAIRED_MARGIN_MS);
    let minimum_wrapper_ms = subject_budget_ms
        .checked_add(scheduling_margin_ms)
        .ok_or_else(|| "paired timeout budget plus scheduling margin overflows".to_owned())?;
    if spec.limits.paired_timeout_ms <= minimum_wrapper_ms {
        return Err(format!(
            "paired_timeout_ms must exceed the worst-case paired subject budget of {subject_budget_ms} ms plus a {scheduling_margin_ms} ms scheduling margin (more than {minimum_wrapper_ms} ms for {largest_paired_blocks} blocks x {PAIRED_RUNS_PER_BLOCK} runs x {} ms subject_timeout_ms)",
            spec.limits.subject_timeout_ms
        ));
    }
    Ok(())
}

fn validate_timeout(name: &str, value: u64) -> Result<(), String> {
    if value == 0 || value > MAX_TIMEOUT_MS {
        Err(format!(
            "{name} must be between 1 and {MAX_TIMEOUT_MS} milliseconds"
        ))
    } else {
        Ok(())
    }
}

fn validate_id(id: &str, description: &str) -> Result<(), String> {
    let safe = !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_alphanumeric())
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'));
    if !safe {
        return Err(format!(
            "{description} id `{id}` is unsafe; use 1-128 ASCII letters, digits, '.', '-', or '_', starting with a letter or digit"
        ));
    }
    Ok(())
}

fn validate_args(args: &[String], description: &str) -> Result<(), String> {
    if args.iter().any(|arg| arg.contains('\0')) {
        return Err(format!(
            "{description} arguments must not contain a NUL byte"
        ));
    }
    Ok(())
}

fn ensure_absent(path: &Path, description: &str) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Ok(_) => Err(format!("{description} {} already exists", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!(
            "could not inspect {description} {}: {error}",
            path.display()
        )),
    }
}

fn resolve_file(path: &mut PathBuf, base: &Path, description: &str) -> Result<(), String> {
    let unresolved = if path.is_absolute() {
        path.clone()
    } else {
        base.join(&*path)
    };
    let resolved = fs::canonicalize(&unresolved).map_err(|error| {
        format!(
            "could not resolve {description} {}: {error}",
            unresolved.display()
        )
    })?;
    let metadata = fs::metadata(&resolved).map_err(|error| {
        format!(
            "could not inspect {description} {}: {error}",
            resolved.display()
        )
    })?;
    if !metadata.is_file() {
        return Err(format!(
            "{description} {} is not a file",
            resolved.display()
        ));
    }
    *path = resolved;
    Ok(())
}

fn collect_provenance(
    spec: &CampaignSpec,
    spec_path: &Path,
    spec_sha256: &str,
) -> Result<Vec<ProvenanceEntry>, String> {
    let mut entries = vec![ProvenanceEntry {
        kind: "spec".to_owned(),
        id: spec.id.clone(),
        workload_id: None,
        path: spec_path.display().to_string(),
        sha256: spec_sha256.to_owned(),
    }];
    entries.push(provenance_file(
        "paired",
        "optikit-paired",
        None,
        &spec.paired,
    )?);
    entries.push(provenance_file(
        "baseline_binary",
        &spec.baseline.id,
        None,
        &spec.baseline.binary,
    )?);
    for candidate in &spec.candidates {
        entries.push(provenance_file(
            "candidate_binary",
            &candidate.id,
            None,
            &candidate.binary,
        )?);
    }
    for workload in &spec.workloads {
        for (index, artifact) in workload.artifacts.iter().enumerate() {
            entries.push(provenance_file(
                "workload_artifact",
                &format!("artifact-{index}"),
                Some(&workload.id),
                artifact,
            )?);
        }
    }
    Ok(entries)
}

fn provenance_file(
    kind: &str,
    id: &str,
    workload_id: Option<&str>,
    path: &Path,
) -> Result<ProvenanceEntry, String> {
    Ok(ProvenanceEntry {
        kind: kind.to_owned(),
        id: id.to_owned(),
        workload_id: workload_id.map(str::to_owned),
        path: path.display().to_string(),
        sha256: sha256_file(path)?,
    })
}

fn sha256_file(path: &Path) -> Result<String, String> {
    let mut file = File::open(path)
        .map_err(|error| format!("could not open {} for hashing: {error}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| format!("could not hash {}: {error}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn sha256_bytes(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn execute(spec: &CampaignSpec, context: &mut RunContext) -> Result<(), String> {
    context.set_phase(
        "baseline_gate",
        json!({"baseline": spec.baseline.id, "workloads": spec.workloads.len()}),
    )?;
    let baseline_gate = gate_artifact(spec, context, &spec.baseline, "original_baseline")?;
    if !baseline_gate.passed {
        return Err(format!(
            "original baseline equivalence gate failed: {}",
            baseline_gate.failures.join("; ")
        ));
    }

    context.set_phase(
        "calibration",
        json!({
            "baseline": spec.baseline.id,
            "order_seed": spec.calibration.order_seed,
            "seeds": spec.calibration.seeds,
            "target_speedup_percent": spec.calibration.target_speedup_percent,
        }),
    )?;
    for workload in &spec.workloads {
        let (summary, recommended_blocks) = run_calibration(spec, context, workload)?;
        let selected_blocks = recommended_blocks.clamp(workload.min_blocks, workload.max_blocks);
        let outcome = CalibrationOutcome {
            workload_id: workload.id.clone(),
            planned_blocks: workload.calibration_blocks,
            recommended_blocks,
            selected_blocks,
            log_ratio_sd: summary.log_ratio_sd,
        };
        context
            .state
            .calibrated_blocks
            .insert(workload.id.clone(), selected_blocks);
        context.state.calibrations.push(outcome.clone());
        context.emit(
            "calibration",
            "calibration_result",
            serde_json::to_value(&outcome)
                .map_err(|error| format!("could not encode calibration result: {error}"))?,
        )?;
        context.checkpoint()?;
        println!(
            "calibration {}: recommended={} selected={}",
            workload.id, recommended_blocks, selected_blocks
        );
    }

    context.set_phase(
        "exploration",
        json!({
            "candidate_order": spec.candidates.iter().map(|candidate| &candidate.id).collect::<Vec<_>>(),
            "order_seed": spec.exploration.order_seed,
            "seeds": spec.exploration.seeds,
        }),
    )?;
    let original_baseline = spec.baseline.clone();
    let mut current_baseline = original_baseline.clone();
    for candidate in &spec.candidates {
        let baseline_before = current_baseline.id.clone();
        context.emit(
            "exploration",
            "candidate_started",
            json!({
                "candidate_id": candidate.id,
                "hypothesis": candidate.hypothesis,
                "baseline": baseline_before,
            }),
        )?;
        let candidate_artifact = candidate.as_artifact();
        let gate = gate_artifact(spec, context, &candidate_artifact, "candidate")?;
        if !gate.passed {
            let outcome = CandidateOutcome {
                candidate_id: candidate.id.clone(),
                hypothesis: candidate.hypothesis.clone(),
                baseline_before: baseline_before.clone(),
                baseline_after: current_baseline.id.clone(),
                decision: "gate_failed".to_owned(),
                gate_failures: gate.failures,
                workloads: Vec::new(),
            };
            context.state.candidates.push(outcome.clone());
            context.emit(
                "exploration",
                "candidate_gate_failed",
                serde_json::to_value(&outcome)
                    .map_err(|error| format!("could not encode candidate result: {error}"))?,
            )?;
            context.checkpoint()?;
            println!(
                "candidate {}: gate_failed (baseline retained: {})",
                candidate.id, current_baseline.id
            );
            continue;
        }
        let results = run_comparison(
            spec,
            context,
            &current_baseline,
            &candidate_artifact,
            &spec.exploration,
            false,
            "exploration",
        )?;
        let promoted = results
            .iter()
            .all(|result| result.lower_95_one_sided_ratio > spec.decision.min_lower_bound_ratio);
        if promoted {
            current_baseline = candidate_artifact;
            context.state.current_baseline = current_baseline.id.clone();
            context.state.promotions.push(BaselineTransition {
                candidate_id: candidate.id.clone(),
                from: baseline_before.clone(),
                to: current_baseline.id.clone(),
            });
        }
        let outcome = CandidateOutcome {
            candidate_id: candidate.id.clone(),
            hypothesis: candidate.hypothesis.clone(),
            baseline_before: baseline_before.clone(),
            baseline_after: current_baseline.id.clone(),
            decision: if promoted { "promoted" } else { "not_promoted" }.to_owned(),
            gate_failures: Vec::new(),
            workloads: results,
        };
        context.state.candidates.push(outcome.clone());
        context.emit(
            "exploration",
            if promoted {
                "baseline_promoted"
            } else {
                "baseline_retained"
            },
            serde_json::to_value(&outcome)
                .map_err(|error| format!("could not encode candidate result: {error}"))?,
        )?;
        context.checkpoint()?;
        println!(
            "candidate {}: {} (baseline: {} -> {})",
            candidate.id, outcome.decision, baseline_before, current_baseline.id
        );
    }

    if context.state.promotions.is_empty() {
        context.set_phase(
            "confirmation_not_applicable",
            json!({"reason": "no_candidate_promoted"}),
        )?;
        context.verify_all_provenance("confirmation_not_applicable")?;
        context.complete("no_candidate_promoted")?;
        return Ok(());
    }

    context.set_phase(
        "confirmation",
        json!({
            "original_baseline": original_baseline.id,
            "final_baseline": current_baseline.id,
            "order_seed": spec.confirmation.order_seed,
            "seeds": spec.confirmation.seeds,
            "one_shot": true,
        }),
    )?;
    let results = run_comparison(
        spec,
        context,
        &original_baseline,
        &current_baseline,
        &spec.confirmation,
        true,
        "confirmation",
    )?;
    let confirmed = results
        .iter()
        .all(|result| result.lower_95_one_sided_ratio > spec.decision.min_lower_bound_ratio);
    let confirmation = ConfirmationOutcome {
        original_baseline: original_baseline.id.clone(),
        final_baseline: current_baseline.id.clone(),
        decision: if confirmed {
            "confirmed"
        } else {
            "inconclusive"
        }
        .to_owned(),
        workloads: results,
    };
    context.state.confirmation = Some(confirmation.clone());
    context.emit(
        "confirmation",
        "confirmation_result",
        serde_json::to_value(&confirmation)
            .map_err(|error| format!("could not encode confirmation result: {error}"))?,
    )?;
    context.checkpoint()?;
    context.verify_all_provenance("confirmation")?;
    let outcome = if confirmed {
        "confirmed"
    } else {
        "confirmation_inconclusive"
    };
    context.complete(outcome)
}

struct GateOutcome {
    passed: bool,
    failures: Vec<String>,
}

fn gate_artifact(
    spec: &CampaignSpec,
    context: &mut RunContext,
    artifact: &ArtifactSpec,
    role: &str,
) -> Result<GateOutcome, String> {
    let mut failures = Vec::new();
    for workload in &spec.workloads {
        let args = artifact
            .args
            .iter()
            .chain(&workload.gate_args)
            .cloned()
            .collect::<Vec<_>>();
        let label = format!("{role}-{}-{}", artifact.id, workload.id);
        let phase = context.state.phase.clone();
        context.verify_workload_artifacts(&phase, workload)?;
        let output = context.capture(
            &phase,
            "gate",
            &label,
            &artifact.binary,
            &args,
            spec.limits.gate_timeout_ms,
        )?;
        context.emit(
            &phase,
            "gate_result",
            json!({
                "role": role,
                "artifact_id": artifact.id,
                "workload_id": workload.id,
                "passed": output.success,
                "status": output.status,
                "stdout_path": output.stdout_path,
                "stderr_path": output.stderr_path,
            }),
        )?;
        context.checkpoint()?;
        if !output.success && output.status != "1" {
            return Err(format!(
                "gate for {} on {} failed operationally with {}; {}",
                artifact.id,
                workload.id,
                output.status,
                capture_failure_detail(&output)
            ));
        }
        if !output.success {
            failures.push(format!(
                "{}:{} exited with {}; stderr: {}",
                artifact.id,
                workload.id,
                output.status,
                compact_output(&output.stderr)
            ));
        }
    }
    Ok(GateOutcome {
        passed: failures.is_empty(),
        failures,
    })
}

struct ParsedPairedResult {
    lower_95_one_sided_ratio: f64,
    speedup_percent: f64,
    evidence: String,
    log_ratio_sd: f64,
}

fn run_calibration(
    spec: &CampaignSpec,
    context: &mut RunContext,
    workload: &WorkloadSpec,
) -> Result<(ParsedPairedResult, usize), String> {
    context.verify_workload_artifacts("calibration", workload)?;
    context.verify_file("calibration", &spec.baseline.binary, "calibration_subject")?;
    let mut args = vec![
        "--aa".to_owned(),
        spec.baseline.binary.display().to_string(),
        "--measure".to_owned(),
        spec.measure.clone(),
        "--direct-args".to_owned(),
        "--timeout-ms".to_owned(),
        spec.limits.subject_timeout_ms.to_string(),
        "--max-output-bytes".to_owned(),
        spec.limits.max_output_bytes.to_string(),
    ];
    append_exact_args(
        &mut args,
        "--subject-arg",
        spec.baseline.args.iter().chain(&workload.args),
    );
    append_design_args(
        &mut args,
        workload,
        workload.calibration_blocks,
        spec.calibration.order_seed,
        &spec.calibration.seeds,
    );
    args.push("--target-speedup".to_owned());
    args.push(spec.calibration.target_speedup_percent.to_string());
    let output = context.capture(
        "calibration",
        "paired-aa",
        &workload.id,
        &spec.paired,
        &args,
        spec.limits.paired_timeout_ms,
    )?;
    ensure_paired_success(&output, &format!("A/A calibration `{}`", workload.id))?;
    let stdout = paired_stdout(&output, &format!("A/A calibration `{}`", workload.id))?;
    let result = parse_paired_result(
        stdout,
        "AA",
        "calibration",
        &spec.measure,
        workload.calibration_blocks,
    )?;
    let recommended = parse_calibration(stdout)?;
    Ok((result, recommended))
}

fn run_comparison(
    spec: &CampaignSpec,
    context: &mut RunContext,
    baseline: &ArtifactSpec,
    candidate: &ArtifactSpec,
    design: &DesignSpec,
    held_out: bool,
    phase: &str,
) -> Result<Vec<ResultSummary>, String> {
    let mut results = Vec::with_capacity(spec.workloads.len());
    for workload in &spec.workloads {
        context.verify_workload_artifacts(phase, workload)?;
        context.verify_file(phase, &baseline.binary, "comparison_baseline")?;
        context.verify_file(phase, &candidate.binary, "comparison_candidate")?;
        let blocks = *context
            .state
            .calibrated_blocks
            .get(&workload.id)
            .ok_or_else(|| format!("workload `{}` has no calibrated block count", workload.id))?;
        let mut args = vec![
            "--baseline".to_owned(),
            baseline.binary.display().to_string(),
            "--candidate".to_owned(),
            candidate.binary.display().to_string(),
            "--measure".to_owned(),
            spec.measure.clone(),
            "--direct-args".to_owned(),
            "--timeout-ms".to_owned(),
            spec.limits.subject_timeout_ms.to_string(),
            "--max-output-bytes".to_owned(),
            spec.limits.max_output_bytes.to_string(),
        ];
        append_exact_args(
            &mut args,
            "--baseline-arg",
            baseline.args.iter().chain(&workload.args),
        );
        append_exact_args(
            &mut args,
            "--candidate-arg",
            candidate.args.iter().chain(&workload.args),
        );
        append_design_args(
            &mut args,
            workload,
            blocks,
            design.order_seed,
            &design.seeds,
        );
        if held_out {
            args.push("--held-out".to_owned());
        }
        let label = format!("{}-vs-{}-{}", candidate.id, baseline.id, workload.id);
        let output = context.capture(
            phase,
            "paired-ab",
            &label,
            &spec.paired,
            &args,
            spec.limits.paired_timeout_ms,
        )?;
        ensure_paired_success(
            &output,
            &format!(
                "paired comparison `{}` vs `{}` on `{}`",
                candidate.id, baseline.id, workload.id
            ),
        )?;
        let stdout = paired_stdout(
            &output,
            &format!(
                "paired comparison `{}` vs `{}` on `{}`",
                candidate.id, baseline.id, workload.id
            ),
        )?;
        let expected_scope = if held_out {
            "held_out_confirmation"
        } else {
            "exploratory_per_candidate"
        };
        let parsed = parse_paired_result(stdout, "AB", expected_scope, &spec.measure, blocks)?;
        let result = ResultSummary {
            workload_id: workload.id.clone(),
            blocks,
            lower_95_one_sided_ratio: parsed.lower_95_one_sided_ratio,
            speedup_percent: parsed.speedup_percent,
            evidence: parsed.evidence,
        };
        context.emit(
            phase,
            "workload_result",
            json!({
                "baseline_id": baseline.id,
                "candidate_id": candidate.id,
                "held_out": held_out,
                "result": result,
                "threshold": spec.decision.min_lower_bound_ratio,
            }),
        )?;
        context.checkpoint()?;
        results.push(result);
    }
    Ok(results)
}

fn append_exact_args<'a>(
    destination: &mut Vec<String>,
    option: &str,
    values: impl Iterator<Item = &'a String>,
) {
    for value in values {
        destination.push(option.to_owned());
        destination.push(value.clone());
    }
}

fn append_design_args(
    destination: &mut Vec<String>,
    workload: &WorkloadSpec,
    blocks: usize,
    order_seed: u64,
    seeds: &[u64],
) {
    destination.extend([
        "--count".to_owned(),
        workload.count.to_string(),
        "--sessions".to_owned(),
        workload.sessions.to_string(),
        "--blocks".to_owned(),
        blocks.to_string(),
        "--order-seed".to_owned(),
        order_seed.to_string(),
    ]);
    for seed in seeds {
        destination.push("--seed".to_owned());
        destination.push(seed.to_string());
    }
}

fn ensure_paired_success(output: &CapturedOutput, description: &str) -> Result<(), String> {
    if output.success {
        Ok(())
    } else {
        Err(format!(
            "{description} failed with {}; {} (raw: {}, {})",
            output.status,
            capture_failure_detail(output),
            output.stdout_path,
            output.stderr_path
        ))
    }
}

fn capture_failure_detail(output: &CapturedOutput) -> String {
    let mut details = Vec::new();
    if let Some(error) = &output.operational_error {
        details.push(error.clone());
    }
    let stderr = compact_output(&output.stderr);
    if !stderr.is_empty() {
        details.push(format!("stderr: {stderr}"));
    }
    if details.is_empty() {
        "no stderr diagnostics".to_owned()
    } else {
        details.join("; ")
    }
}

fn paired_stdout<'a>(output: &'a CapturedOutput, description: &str) -> Result<&'a str, String> {
    std::str::from_utf8(&output.stdout)
        .map_err(|_| format!("{description} emitted non-UTF-8 stdout"))
}

fn parse_paired_result(
    stdout: &str,
    expected_experiment: &str,
    expected_scope: &str,
    expected_measure: &str,
    expected_blocks: usize,
) -> Result<ParsedPairedResult, String> {
    let lines = stdout
        .lines()
        .filter(|line| *line == "RESULT" || line.starts_with("RESULT "))
        .collect::<Vec<_>>();
    if lines.len() != 1 {
        return Err(format!(
            "paired output contained {} RESULT lines; expected exactly one",
            lines.len()
        ));
    }
    let line = lines[0];
    let experiment = required_field(line, "experiment")?;
    if experiment != expected_experiment {
        return Err(format!(
            "paired RESULT experiment `{experiment}` does not match `{expected_experiment}`"
        ));
    }
    let scope = required_field(line, "scope")?;
    if scope != expected_scope {
        return Err(format!(
            "paired RESULT scope `{scope}` does not match `{expected_scope}`"
        ));
    }
    let measure = required_field(line, "mode")?;
    if measure != expected_measure {
        return Err(format!(
            "paired RESULT mode `{measure}` does not match `{expected_measure}`"
        ));
    }
    let valid_blocks = parse_field::<usize>(line, "valid_blocks")?;
    let planned_blocks = parse_field::<usize>(line, "planned_blocks")?;
    let invalid_blocks = parse_field::<usize>(line, "invalid_blocks")?;
    if valid_blocks != expected_blocks || planned_blocks != expected_blocks || invalid_blocks != 0 {
        return Err(format!(
            "paired RESULT is invalid: valid_blocks={valid_blocks}, planned_blocks={planned_blocks}, invalid_blocks={invalid_blocks}, expected_blocks={expected_blocks}"
        ));
    }
    let lower_95_one_sided_ratio = parse_finite_field(line, "lower_95_one_sided_ratio")?;
    if lower_95_one_sided_ratio <= 0.0 {
        return Err("paired lower confidence bound must be positive".to_owned());
    }
    let speedup_percent = parse_finite_field(line, "speedup_percent")?;
    let log_ratio_sd = parse_finite_field(line, "log_ratio_sd")?;
    if log_ratio_sd < 0.0 {
        return Err("paired log-ratio standard deviation must not be negative".to_owned());
    }
    let evidence = required_field(line, "evidence")?.to_owned();
    if evidence == "invalid_design" {
        return Err("paired runner reported invalid_design".to_owned());
    }
    if expected_experiment == "AA" && evidence != "calibration_only" {
        return Err(format!(
            "paired A/A RESULT evidence `{evidence}` is not `calibration_only`"
        ));
    }
    if expected_scope == "exploratory_per_candidate"
        && !matches!(evidence.as_str(), "screen_positive" | "screen_inconclusive")
    {
        return Err(format!(
            "paired exploratory RESULT has invalid evidence `{evidence}`"
        ));
    }
    if expected_scope == "held_out_confirmation"
        && !matches!(evidence.as_str(), "candidate_faster" | "inconclusive")
    {
        return Err(format!(
            "paired confirmation RESULT has invalid evidence `{evidence}`"
        ));
    }
    Ok(ParsedPairedResult {
        lower_95_one_sided_ratio,
        speedup_percent,
        evidence,
        log_ratio_sd,
    })
}

fn parse_calibration(stdout: &str) -> Result<usize, String> {
    let lines = stdout
        .lines()
        .filter(|line| *line == "CALIBRATION" || line.starts_with("CALIBRATION "))
        .collect::<Vec<_>>();
    if lines.len() != 1 {
        return Err(format!(
            "paired output contained {} CALIBRATION lines; expected exactly one",
            lines.len()
        ));
    }
    let blocks = parse_field::<usize>(lines[0], "approximate_blocks_for_80_percent_power")?;
    if blocks == 0 {
        return Err("paired calibration recommended zero blocks".to_owned());
    }
    Ok(blocks)
}

fn field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let prefix = format!("{key}=");
    line.split_whitespace()
        .find_map(|token| token.strip_prefix(&prefix))
}

fn required_field<'a>(line: &'a str, key: &str) -> Result<&'a str, String> {
    field(line, key).ok_or_else(|| format!("paired output missing `{key}`"))
}

fn parse_field<T>(line: &str, key: &str) -> Result<T, String>
where
    T: std::str::FromStr,
{
    required_field(line, key)?
        .parse::<T>()
        .map_err(|_| format!("paired field `{key}` is invalid"))
}

fn parse_finite_field(line: &str, key: &str) -> Result<f64, String> {
    let value = parse_field::<f64>(line, key)?;
    if value.is_finite() {
        Ok(value)
    } else {
        Err(format!("paired field `{key}` must be finite"))
    }
}

fn render_report(state: &CampaignState) -> String {
    let mut report = String::new();
    report.push_str(&format!("# Campaign {}\n\n", state.campaign_id));
    report.push_str(&format!("- Status: `{}`\n", state.status));
    report.push_str(&format!(
        "- Outcome: `{}`\n",
        state.outcome.as_deref().unwrap_or("pending")
    ));
    report.push_str(&format!(
        "- Original baseline: `{}`\n",
        state.original_baseline
    ));
    report.push_str(&format!("- Final baseline: `{}`\n", state.current_baseline));
    report.push_str(&format!(
        "- Promotion threshold: lower 95% ratio `> {:.6}`\n",
        state.min_lower_bound_ratio
    ));
    if let Some(error) = &state.error {
        report.push_str(&format!("- Error: {}\n", markdown_cell(error)));
    }

    report.push_str("\n## Calibration\n\n");
    if state.calibrations.is_empty() {
        report.push_str("No calibration completed.\n");
    } else {
        report.push_str("| Workload | Planned | Recommended | Selected | Log-ratio SD |\n");
        report.push_str("|---|---:|---:|---:|---:|\n");
        for calibration in &state.calibrations {
            report.push_str(&format!(
                "| {} | {} | {} | {} | {:.9} |\n",
                calibration.workload_id,
                calibration.planned_blocks,
                calibration.recommended_blocks,
                calibration.selected_blocks,
                calibration.log_ratio_sd
            ));
        }
    }

    report.push_str("\n## Candidate exploration\n\n");
    if state.candidates.is_empty() {
        report.push_str("No candidate comparison completed.\n");
    } else {
        for candidate in &state.candidates {
            report.push_str(&format!(
                "### {} — {}\n\n",
                candidate.candidate_id, candidate.decision
            ));
            report.push_str(&format!(
                "Hypothesis: {}\n\n",
                markdown_cell(&candidate.hypothesis)
            ));
            report.push_str(&format!(
                "Baseline transition: `{}` → `{}`\n\n",
                candidate.baseline_before, candidate.baseline_after
            ));
            if candidate.gate_failures.is_empty() {
                render_results_table(&mut report, &candidate.workloads);
            } else {
                report.push_str("Gate failures:\n\n");
                for failure in &candidate.gate_failures {
                    report.push_str(&format!("- {}\n", markdown_cell(failure)));
                }
                report.push('\n');
            }
        }
    }

    report.push_str("\n## Baseline transitions\n\n");
    if state.promotions.is_empty() {
        report.push_str("No candidate was promoted.\n");
    } else {
        for transition in &state.promotions {
            report.push_str(&format!(
                "- `{}`: `{}` → `{}`\n",
                transition.candidate_id, transition.from, transition.to
            ));
        }
    }

    report.push_str("\n## Held-out confirmation\n\n");
    match &state.confirmation {
        Some(confirmation) => {
            report.push_str(&format!(
                "Decision: `{}` (`{}` vs `{}`).\n\n",
                confirmation.decision, confirmation.final_baseline, confirmation.original_baseline
            ));
            render_results_table(&mut report, &confirmation.workloads);
        }
        None if state.promotions.is_empty() && state.status == "completed" => {
            report.push_str("Not run because no candidate was promoted.\n")
        }
        None => report.push_str("Not completed.\n"),
    }
    report
}

fn render_results_table(report: &mut String, results: &[ResultSummary]) {
    report.push_str("| Workload | Blocks | Lower 95% ratio | Speedup | Evidence |\n");
    report.push_str("|---|---:|---:|---:|---|\n");
    for result in results {
        report.push_str(&format!(
            "| {} | {} | {:.6} | {:.3}% | {} |\n",
            result.workload_id,
            result.blocks,
            result.lower_95_one_sided_ratio,
            result.speedup_percent,
            markdown_cell(&result.evidence)
        ));
    }
    report.push('\n');
}

fn markdown_cell(value: &str) -> String {
    value
        .replace('|', "\\|")
        .replace(['\r', '\n'], " ")
        .trim()
        .to_owned()
}

fn timestamp() -> String {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(duration) => format!("{}.{:03}", duration.as_secs(), duration.subsec_millis()),
        Err(_) => "0.000".to_owned(),
    }
}

fn compact_output(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .trim()
        .chars()
        .map(|character| {
            if matches!(character, '\n' | '\r' | '\t') {
                ' '
            } else {
                character
            }
        })
        .take(300)
        .collect()
}

fn safe_filename_component(value: &str) -> String {
    value
        .bytes()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.') {
                char::from(byte)
            } else {
                '_'
            }
        })
        .take(180)
        .collect()
}

fn write_new_sync(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|error| format!("could not create {}: {error}", path.display()))?;
    file.write_all(bytes)
        .map_err(|error| format!("could not write {}: {error}", path.display()))?;
    file.sync_all()
        .map_err(|error| format!("could not sync {}: {error}", path.display()))
}

fn write_atomic(directory: &Path, name: &str, bytes: &[u8]) -> Result<(), String> {
    let temporary = directory.join(format!(".{name}.tmp"));
    let destination = directory.join(name);
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&temporary)
        .map_err(|error| format!("could not create {}: {error}", temporary.display()))?;
    file.write_all(bytes)
        .map_err(|error| format!("could not write {}: {error}", temporary.display()))?;
    file.sync_all()
        .map_err(|error| format!("could not sync {}: {error}", temporary.display()))?;
    fs::rename(&temporary, &destination).map_err(|error| {
        format!(
            "could not atomically replace {}: {error}",
            destination.display()
        )
    })?;
    sync_directory(directory)
}

fn sync_directory(path: &Path) -> Result<(), String> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| format!("could not sync directory {}: {error}", path.display()))
}

fn run() -> Result<(), String> {
    let cli = match parse_args(env::args().skip(1))? {
        Some(cli) => cli,
        None => {
            println!("{USAGE}");
            return Ok(());
        }
    };
    let (mut spec, spec_bytes, spec_path) = parse_spec(&cli.spec)?;
    validate(&mut spec, &cli.run_dir)?;
    let spec_sha256 = sha256_bytes(&spec_bytes);
    let provenance = collect_provenance(&spec, &spec_path, &spec_sha256)?;
    let mut context =
        RunContext::create(&cli.run_dir, &spec, &spec_bytes, &spec_sha256, &provenance)?;
    match execute(&spec, &mut context) {
        Ok(()) => {
            println!(
                "campaign {}: {} (final baseline: {})",
                spec.id,
                context.state.outcome.as_deref().unwrap_or("completed"),
                context.state.current_baseline
            );
            Ok(())
        }
        Err(error) => {
            if let Err(persist_error) = context.abort(&error) {
                return Err(format!(
                    "{error}; additionally could not persist campaign failure: {persist_error}"
                ));
            }
            Err(error)
        }
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("optikit-campaign: {error}\n{USAGE}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_ids_are_strict() {
        assert!(validate_id("candidate-1.alpha", "candidate").is_ok());
        assert!(validate_id("../candidate", "candidate").is_err());
        assert!(validate_id("candidate name", "candidate").is_err());
        assert!(validate_id("_candidate", "candidate").is_err());
    }

    #[test]
    fn parses_complete_paired_result() {
        let output = "RESULT experiment=AB scope=exploratory_per_candidate mode=scan valid_blocks=4 planned_blocks=4 invalid_blocks=0 mean_log_ratio=0.1 log_ratio_sd=0.02 speedup_ratio=1.1 speedup_percent=10.0 lower_95_one_sided_ratio=1.01 evidence=screen_positive\n";
        let result =
            parse_paired_result(output, "AB", "exploratory_per_candidate", "scan", 4).unwrap();
        assert_eq!(result.lower_95_one_sided_ratio, 1.01);
        assert_eq!(result.speedup_percent, 10.0);
        assert_eq!(result.log_ratio_sd, 0.02);
    }

    #[test]
    fn rejects_partial_or_invalid_paired_result() {
        let invalid = "RESULT experiment=AB valid_blocks=3 planned_blocks=4 invalid_blocks=1 log_ratio_sd=0.1 speedup_percent=1 lower_95_one_sided_ratio=1.1 evidence=invalid_design\n";
        assert!(
            parse_paired_result(invalid, "AB", "exploratory_per_candidate", "scan", 4).is_err()
        );
        assert!(parse_paired_result("", "AB", "exploratory_per_candidate", "scan", 4).is_err());
    }

    #[test]
    fn parses_calibration_recommendation() {
        assert_eq!(
            parse_calibration(
                "CALIBRATION target_speedup_percent=3 approximate_blocks_for_80_percent_power=17\n"
            )
            .unwrap(),
            17
        );
    }
}
