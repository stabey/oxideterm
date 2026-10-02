use std::{
    collections::VecDeque,
    fs::{self, File, OpenOptions},
    io::{self, BufWriter, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
        mpsc::{self, RecvTimeoutError, SyncSender, TrySendError},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant, SystemTime},
};

use chrono::{DateTime, Local};
use oxideterm_settings::{
    ParsedTerminalSessionLogDirectoryTemplate, ParsedTerminalSessionLogTemplate,
    TerminalSessionLogFileMode, TerminalSessionLogTemplatePart, TerminalSessionLogTemplateVariable,
    parse_terminal_session_log_content_template, parse_terminal_session_log_directory_template,
    parse_terminal_session_log_file_name_template,
};
use zeroize::Zeroizing;

const SESSION_LOG_CHUNK_BYTES: usize = 64 * 1024;
const SESSION_LOG_BUFFER_BYTES: usize = 16 * 1024 * 1024;
const SESSION_LOG_FLUSH_INTERVAL: Duration = Duration::from_millis(250);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TerminalSessionLogState {
    Idle,
    Logging,
    Paused,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TerminalSessionLogStatus {
    pub state: TerminalSessionLogState,
    pub path: Option<PathBuf>,
    pub bytes_written: u64,
    pub failed: bool,
}

impl Default for TerminalSessionLogStatus {
    fn default() -> Self {
        Self {
            state: TerminalSessionLogState::Idle,
            path: None,
            bytes_written: 0,
            failed: false,
        }
    }
}

#[derive(Clone)]
pub struct TerminalSessionLogOptions {
    pub directory: PathBuf,
    pub directory_template: String,
    pub include_control_sequences: bool,
    pub retention_days: u64,
    pub max_file_bytes: Option<u64>,
    pub file_name_template: String,
    pub content_template: String,
    pub file_mode: TerminalSessionLogFileMode,
    pub context: TerminalSessionLogContext,
}

#[derive(Clone, Default)]
pub struct TerminalSessionLogContext {
    pub session: String,
    pub host: String,
    pub username: String,
    pub protocol: String,
}

enum SessionLogCommand {
    Output,
    Flush(SyncSender<bool>),
    Finish,
}

#[derive(Default)]
struct SessionLogOutput {
    chunks: VecDeque<Zeroizing<Vec<u8>>>,
    bytes: usize,
}

impl SessionLogOutput {
    fn append(&mut self, mut bytes: &[u8]) -> io::Result<()> {
        if bytes.len() > SESSION_LOG_BUFFER_BYTES.saturating_sub(self.bytes) {
            tracing::warn!(
                buffered_bytes = self.bytes,
                incoming_bytes = bytes.len(),
                capacity_bytes = SESSION_LOG_BUFFER_BYTES,
                "terminal session log writer is overloaded"
            );
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "terminal session log writer is overloaded",
            ));
        }
        self.bytes += bytes.len();
        while !bytes.is_empty() {
            if self
                .chunks
                .back()
                .is_none_or(|chunk| chunk.len() == SESSION_LOG_CHUNK_BYTES)
            {
                self.chunks.push_back(Zeroizing::new(Vec::new()));
            }
            let chunk = self.chunks.back_mut().unwrap();
            let count = bytes.len().min(SESSION_LOG_CHUNK_BYTES - chunk.len());
            let required = chunk.len() + count;
            if chunk.capacity() < required {
                // Growing by replacement wipes the old allocation; sparse output stays inexpensive.
                let mut grown = Zeroizing::new(Vec::with_capacity(required.next_power_of_two()));
                grown.extend_from_slice(chunk);
                *chunk = grown;
            }
            chunk.extend_from_slice(&bytes[..count]);
            bytes = &bytes[count..];
        }
        Ok(())
    }
}

#[derive(Default)]
struct SessionLogFailure {
    failed: bool,
    wakeup: Option<async_channel::Sender<()>>,
}

pub struct TerminalSessionLog {
    state: TerminalSessionLogState,
    path: PathBuf,
    sender: Option<SyncSender<SessionLogCommand>>,
    output: Arc<Mutex<SessionLogOutput>>,
    worker: Option<JoinHandle<io::Result<()>>>,
    bytes_written: Arc<AtomicU64>,
    failure: Arc<Mutex<SessionLogFailure>>,
}

pub fn prune_terminal_session_logs(directory: &Path, retention_days: u64) -> io::Result<()> {
    if !directory.exists() {
        return Ok(());
    }
    remove_expired_logs(directory, retention_days)
}

impl TerminalSessionLog {
    pub fn start(options: TerminalSessionLogOptions) -> io::Result<Self> {
        fs::create_dir_all(&options.directory)?;
        remove_expired_logs(&options.directory, options.retention_days)?;
        let directory_template =
            parse_terminal_session_log_directory_template(&options.directory_template)
                .map_err(|_| io::Error::other("invalid terminal session log directory template"))?;
        let file_name_template =
            parse_terminal_session_log_file_name_template(&options.file_name_template)
                .map_err(|_| io::Error::other("invalid terminal session log file name template"))?;
        let content_template =
            parse_terminal_session_log_content_template(&options.content_template)
                .map_err(|_| io::Error::other("invalid terminal session log content template"))?;
        let session_directory = create_session_log_directory(
            &options.directory,
            &directory_template,
            &options.context,
        )?;
        let (path, file, initial_bytes) = create_log_file(
            &session_directory,
            &file_name_template,
            &options.context,
            options.file_mode,
        )?;
        Self::spawn_writer(path, file, initial_bytes, options, content_template)
    }

    pub fn start_at_path(path: PathBuf, options: TerminalSessionLogOptions) -> io::Result<Self> {
        let content_template =
            parse_terminal_session_log_content_template(&options.content_template)
                .map_err(|_| io::Error::other("invalid terminal session log content template"))?;
        // The save dialog authorizes this exact file. User-selected folders must never be pruned
        // or expanded using the automatic log directory and file-name templates.
        let file = open_log_file(&path, TerminalSessionLogFileMode::Overwrite)?;
        Self::spawn_writer(path, file, 0, options, content_template)
    }

    fn spawn_writer(
        path: PathBuf,
        file: File,
        initial_bytes: u64,
        options: TerminalSessionLogOptions,
        content_template: ParsedTerminalSessionLogTemplate,
    ) -> io::Result<Self> {
        if options
            .max_file_bytes
            .is_some_and(|max_file_bytes| initial_bytes >= max_file_bytes)
        {
            return Err(io::Error::other(
                "terminal session log already reached its size limit",
            ));
        }
        // The channel carries wakeups and barriers, not one slot per terminal fragment.
        let (sender, receiver) = mpsc::sync_channel(1);
        let output = Arc::new(Mutex::new(SessionLogOutput::default()));
        let worker_output = output.clone();
        let bytes_written = Arc::new(AtomicU64::new(initial_bytes));
        let failure = Arc::new(Mutex::new(SessionLogFailure::default()));
        let worker_bytes_written = bytes_written.clone();
        let worker_failure = failure.clone();
        let worker = thread::Builder::new()
            .name("terminal-session-log".to_string())
            .spawn(move || {
                let result = run_session_log_writer(
                    file,
                    receiver,
                    worker_output,
                    worker_bytes_written,
                    options.include_control_sequences,
                    options.max_file_bytes,
                    content_template,
                    options.context,
                );
                if let Err(error) = &result {
                    // Error categories are diagnostic data; terminal contents and paths are not.
                    tracing::warn!(
                        error_kind = ?error.kind(),
                        os_error = error.raw_os_error(),
                        "terminal session log writer failed"
                    );
                }
                if result.is_err()
                    && let Ok(mut failure) = worker_failure.lock()
                {
                    failure.failed = true;
                    if let Some(wakeup) = &failure.wakeup {
                        // A timed flush can fail after the terminal has gone idle.
                        let _ = wakeup.try_send(());
                    }
                }
                result
            })?;

        Ok(Self {
            state: TerminalSessionLogState::Logging,
            path,
            sender: Some(sender),
            output,
            worker: Some(worker),
            bytes_written,
            failure,
        })
    }

    pub fn status(&self) -> TerminalSessionLogStatus {
        let failed = self.has_failed();
        TerminalSessionLogStatus {
            state: if failed {
                TerminalSessionLogState::Idle
            } else {
                self.state
            },
            path: Some(self.path.clone()),
            bytes_written: self.bytes_written.load(Ordering::Relaxed),
            failed,
        }
    }

    pub(crate) fn has_failed(&self) -> bool {
        self.failure.lock().is_ok_and(|failure| failure.failed)
    }

    pub(crate) fn wake_on_failure(&self, wakeup: async_channel::Sender<()>) {
        if let Ok(mut failure) = self.failure.lock() {
            if failure.failed {
                let _ = wakeup.try_send(());
            }
            failure.wakeup = Some(wakeup);
        }
    }

    pub fn pause(&mut self) -> io::Result<()> {
        if self.state != TerminalSessionLogState::Logging {
            return Ok(());
        }
        self.flush()?;
        self.state = TerminalSessionLogState::Paused;
        Ok(())
    }

    pub fn resume(&mut self) {
        if self.state == TerminalSessionLogState::Paused {
            self.state = TerminalSessionLogState::Logging;
        }
    }

    pub fn flush(&self) -> io::Result<()> {
        let (acknowledge, acknowledgement) = mpsc::sync_channel(0);
        self.send(SessionLogCommand::Flush(acknowledge))?;
        match acknowledgement.recv() {
            Ok(true) => Ok(()),
            _ => Err(io::Error::other(
                "terminal session log could not be flushed",
            )),
        }
    }

    pub fn write_output(&mut self, bytes: Vec<u8>) -> io::Result<()> {
        // Queued terminal output is owned only until this file consumer writes or rejects it.
        let bytes = Zeroizing::new(bytes);
        if self.state != TerminalSessionLogState::Logging || bytes.is_empty() {
            return Ok(());
        }
        if self.has_failed() {
            return Err(io::Error::other("terminal session log writer failed"));
        }

        self.output
            .lock()
            .map_err(|_| io::Error::other("terminal session log buffer unavailable"))?
            .append(&bytes)?;
        // A full channel already holds a wakeup. No output is discarded or blocked on disk I/O.
        match self
            .sender
            .as_ref()
            .ok_or_else(|| io::Error::other("terminal session log writer stopped"))?
            .try_send(SessionLogCommand::Output)
        {
            Ok(()) | Err(TrySendError::Full(_)) => Ok(()),
            Err(TrySendError::Disconnected(_)) => {
                Err(io::Error::other("terminal session log writer stopped"))
            }
        }
    }

    pub fn finish(mut self) -> io::Result<PathBuf> {
        let sender = self.sender.take();
        if let Some(sender) = sender {
            sender
                .send(SessionLogCommand::Finish)
                .map_err(|_| io::Error::other("terminal session log writer stopped"))?;
        }
        let result = self.join_worker();
        if result.is_ok() {
            Ok(self.path.clone())
        } else {
            result.map(|()| self.path.clone())
        }
    }

    fn send(&self, command: SessionLogCommand) -> io::Result<()> {
        self.sender
            .as_ref()
            .ok_or_else(|| io::Error::other("terminal session log writer stopped"))?
            .send(command)
            .map_err(|_| io::Error::other("terminal session log writer stopped"))
    }

    fn join_worker(&mut self) -> io::Result<()> {
        self.sender.take();
        let Some(worker) = self.worker.take() else {
            return Ok(());
        };
        worker
            .join()
            .map_err(|_| io::Error::other("terminal session log writer panicked"))?
    }
}

impl Drop for TerminalSessionLog {
    fn drop(&mut self) {
        if self.worker.is_some() {
            // Closing this pane disconnects the bounded queue and drains accepted output. The
            // writer owns no transport and cannot disconnect another consumer of the SSH node.
            let _ = self.join_worker();
        }
    }
}

fn run_session_log_writer(
    file: File,
    receiver: mpsc::Receiver<SessionLogCommand>,
    output: Arc<Mutex<SessionLogOutput>>,
    bytes_written: Arc<AtomicU64>,
    include_control_sequences: bool,
    max_file_bytes: Option<u64>,
    content_template: ParsedTerminalSessionLogTemplate,
    context: TerminalSessionLogContext,
) -> io::Result<()> {
    let mut writer = BoundedLogWriter::new(file, max_file_bytes, bytes_written);
    let mut printable_filter = PrintableTextFilter::default();
    let mut line_formatter = SessionLogLineFormatter::new(content_template, context)?;
    let mut last_flush = Instant::now();
    let mut dirty = false;

    loop {
        // Only a dirty buffer needs a timer; idle and paused logs do not wake a thread repeatedly.
        let command = if dirty {
            receiver.recv_timeout(SESSION_LOG_FLUSH_INTERVAL.saturating_sub(last_flush.elapsed()))
        } else {
            receiver.recv().map_err(|_| RecvTimeoutError::Disconnected)
        };
        let command = match command {
            Ok(command) => command,
            Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) => {
                writer.flush()?;
                last_flush = Instant::now();
                dirty = false;
                continue;
            }
        };
        match command {
            SessionLogCommand::Output => {
                // Swap under the lock; parsing and disk writes never hold the producer's buffer lock.
                // At most one 16 MiB batch is in flight and one 16 MiB batch is pending.
                let batch =
                    std::mem::take(&mut *output.lock().map_err(|_| {
                        io::Error::other("terminal session log buffer unavailable")
                    })?);
                for bytes in batch.chunks {
                    if include_control_sequences {
                        line_formatter.write(&mut writer, &bytes)?;
                    } else {
                        let printable = printable_filter.filter(&bytes);
                        line_formatter.write(&mut writer, printable.as_bytes())?;
                    }
                    dirty = true;
                    // Continuous output must not postpone flushing until the queue becomes idle.
                    if last_flush.elapsed() >= SESSION_LOG_FLUSH_INTERVAL {
                        writer.flush()?;
                        last_flush = Instant::now();
                        dirty = false;
                    }
                }
            }
            SessionLogCommand::Flush(acknowledge) => match writer.flush() {
                Ok(()) => {
                    last_flush = Instant::now();
                    dirty = false;
                    let _ = acknowledge.send(true);
                }
                Err(error) => {
                    let _ = acknowledge.send(false);
                    return Err(error);
                }
            },
            SessionLogCommand::Finish => {
                line_formatter.finish(&mut writer)?;
                return writer.flush();
            }
        }
    }
    line_formatter.finish(&mut writer)?;
    writer.flush()
}

struct SessionLogLineFormatter {
    prefix: Vec<TerminalSessionLogTemplatePart>,
    suffix: Vec<TerminalSessionLogTemplatePart>,
    context: TerminalSessionLogContext,
    current_line_time: Option<DateTime<Local>>,
    pending_carriage_return: bool,
}

impl SessionLogLineFormatter {
    fn new(
        template: ParsedTerminalSessionLogTemplate,
        context: TerminalSessionLogContext,
    ) -> io::Result<Self> {
        let text_index = template
            .parts()
            .iter()
            .position(|part| {
                *part
                    == TerminalSessionLogTemplatePart::Variable(
                        TerminalSessionLogTemplateVariable::Text,
                    )
            })
            .ok_or_else(|| io::Error::other("terminal session log template has no text field"))?;
        Ok(Self {
            prefix: template.parts()[..text_index].to_vec(),
            suffix: template.parts()[text_index + 1..].to_vec(),
            context,
            current_line_time: None,
            pending_carriage_return: false,
        })
    }

    fn write(&mut self, writer: &mut BoundedLogWriter, content: &[u8]) -> io::Result<()> {
        if content.is_empty() {
            return Ok(());
        }
        let mut index = 0;
        if self.pending_carriage_return {
            if content.first() == Some(&b'\n') {
                self.finish_line(writer, b"\r\n")?;
                index = 1;
            } else if content.first() != Some(&b'\r') {
                // A bare CR returns to the current line; it must not start another template.
                writer.write_all(b"\r")?;
            }
            self.pending_carriage_return = false;
        }

        while index < content.len() {
            let next_break = content[index..]
                .iter()
                .position(|byte| matches!(*byte, b'\r' | b'\n'))
                .map(|offset| index + offset);
            let Some(line_break) = next_break else {
                self.write_text(writer, &content[index..])?;
                return Ok(());
            };
            self.write_text(writer, &content[index..line_break])?;
            if content[line_break] == b'\r' {
                if line_break + 1 >= content.len() {
                    self.pending_carriage_return = true;
                    return Ok(());
                }
                if content[line_break + 1] == b'\n' {
                    self.finish_line(writer, b"\r\n")?;
                    index = line_break + 2;
                } else {
                    // Repeated returns to column zero still belong to the same logical line.
                    if content[line_break + 1] != b'\r' {
                        writer.write_all(b"\r")?;
                    }
                    index = line_break + 1;
                }
            } else {
                self.finish_line(writer, b"\n")?;
                index = line_break + 1;
            }
        }
        Ok(())
    }

    fn write_text(&mut self, writer: &mut BoundedLogWriter, text: &[u8]) -> io::Result<()> {
        self.start_line(writer)?;
        writer.write_all(text)
    }

    fn start_line(&mut self, writer: &mut BoundedLogWriter) -> io::Result<()> {
        if self.current_line_time.is_some() {
            return Ok(());
        }
        let now = Local::now();
        write_template_parts(writer, &self.prefix, &self.context, &now)?;
        self.current_line_time = Some(now);
        Ok(())
    }

    fn finish_line(&mut self, writer: &mut BoundedLogWriter, ending: &[u8]) -> io::Result<()> {
        self.start_line(writer)?;
        let line_time = self.current_line_time.take().unwrap_or_else(Local::now);
        write_template_parts(writer, &self.suffix, &self.context, &line_time)?;
        writer.write_all(ending)
    }

    fn finish(&mut self, writer: &mut BoundedLogWriter) -> io::Result<()> {
        if self.pending_carriage_return {
            self.pending_carriage_return = false;
            self.finish_line(writer, b"\r")?;
        } else if let Some(line_time) = self.current_line_time.take() {
            write_template_parts(writer, &self.suffix, &self.context, &line_time)?;
        }
        Ok(())
    }
}

fn write_template_parts(
    writer: &mut BoundedLogWriter,
    parts: &[TerminalSessionLogTemplatePart],
    context: &TerminalSessionLogContext,
    now: &DateTime<Local>,
) -> io::Result<()> {
    for part in parts {
        match part {
            TerminalSessionLogTemplatePart::Literal(literal) => {
                writer.write_all(literal.as_bytes())?
            }
            TerminalSessionLogTemplatePart::Variable(variable) => {
                let value = template_variable_value(*variable, context, now);
                writer.write_all(value.as_bytes())?;
            }
        }
    }
    Ok(())
}

struct BoundedLogWriter {
    writer: BufWriter<File>,
    max_bytes: Option<u64>,
    bytes_written: Arc<AtomicU64>,
}

impl BoundedLogWriter {
    fn new(file: File, max_bytes: Option<u64>, bytes_written: Arc<AtomicU64>) -> Self {
        Self {
            writer: BufWriter::new(file),
            max_bytes,
            bytes_written,
        }
    }
}

impl Write for BoundedLogWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let written = self.bytes_written.load(Ordering::Relaxed);
        if let Some(max_bytes) = self.max_bytes {
            if written >= max_bytes {
                return Err(io::Error::new(
                    io::ErrorKind::FileTooLarge,
                    "terminal session log reached its size limit",
                ));
            }
            let remaining = (max_bytes - written).min(buffer.len() as u64) as usize;
            if remaining < buffer.len() {
                return Err(io::Error::new(
                    io::ErrorKind::FileTooLarge,
                    "terminal session log reached its size limit",
                ));
            }
        }
        self.writer.write_all(buffer)?;
        self.bytes_written
            .fetch_add(buffer.len() as u64, Ordering::Relaxed);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.writer.flush()
    }
}

#[derive(Default)]
struct PrintableTextFilter {
    parser: vte::Parser,
    output: Zeroizing<String>,
}

impl PrintableTextFilter {
    fn filter(&mut self, bytes: &[u8]) -> Zeroizing<String> {
        let mut performer = PrintableTextCollector {
            output: &mut self.output,
        };
        self.parser.advance(&mut performer, bytes);
        std::mem::take(&mut self.output)
    }
}

struct PrintableTextCollector<'a> {
    output: &'a mut String,
}

impl vte::Perform for PrintableTextCollector<'_> {
    fn print(&mut self, character: char) {
        self.output.push(character);
    }

    fn print_text(&mut self, text: &str) {
        self.output.push_str(text);
    }

    fn execute(&mut self, byte: u8) {
        if matches!(byte, b'\n' | b'\r' | b'\t') {
            self.output.push(byte as char);
        }
    }
}

fn create_log_file(
    directory: &Path,
    template: &ParsedTerminalSessionLogTemplate,
    context: &TerminalSessionLogContext,
    mode: TerminalSessionLogFileMode,
) -> io::Result<(PathBuf, File, u64)> {
    let file_name = render_log_file_name(template, context)?;
    match mode {
        TerminalSessionLogFileMode::Unique => {
            for suffix in 0..1000 {
                let candidate = unique_file_name(&file_name, suffix);
                let path = directory.join(candidate);
                match open_log_file(&path, TerminalSessionLogFileMode::Unique) {
                    Ok(file) => return Ok((path, file, 0)),
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                    Err(error) => return Err(error),
                }
            }
            Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "could not allocate a unique terminal session log name",
            ))
        }
        TerminalSessionLogFileMode::Append | TerminalSessionLogFileMode::Overwrite => {
            let path = directory.join(file_name);
            let file = open_log_file(&path, mode)?;
            let initial_bytes = if mode == TerminalSessionLogFileMode::Append {
                file.metadata()?.len()
            } else {
                0
            };
            Ok((path, file, initial_bytes))
        }
    }
}

fn create_session_log_directory(
    root: &Path,
    template: &ParsedTerminalSessionLogDirectoryTemplate,
    context: &TerminalSessionLogContext,
) -> io::Result<PathBuf> {
    let now = Local::now();
    let mut directory = root.to_path_buf();
    for component_template in template.components() {
        let component = render_log_path_component(component_template, context, &now)?;
        directory.push(component);
        match fs::symlink_metadata(&directory) {
            Ok(metadata) if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() => {
            }
            Ok(_) => {
                return Err(io::Error::other(
                    "terminal session log directory component is not a directory",
                ));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                fs::create_dir(&directory)?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
                }
            }
            Err(error) => return Err(error),
        }
    }
    Ok(directory)
}

fn open_log_file(path: &Path, mode: TerminalSessionLogFileMode) -> io::Result<File> {
    if mode != TerminalSessionLogFileMode::Unique
        && let Ok(metadata) = fs::symlink_metadata(path)
        && !metadata.file_type().is_file()
    {
        // Append and overwrite must never follow a pre-existing link outside the log folder.
        return Err(io::Error::other(
            "terminal session log target is not a regular file",
        ));
    }
    let mut options = OpenOptions::new();
    match mode {
        TerminalSessionLogFileMode::Unique => {
            options.write(true).create_new(true);
        }
        TerminalSessionLogFileMode::Append => {
            options.append(true).create(true);
        }
        TerminalSessionLogFileMode::Overwrite => {
            options.write(true).create(true).truncate(true);
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // Existing append targets are tightened to the same private boundary as new logs.
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    Ok(file)
}

fn render_log_file_name(
    template: &ParsedTerminalSessionLogTemplate,
    context: &TerminalSessionLogContext,
) -> io::Result<String> {
    const MAX_LOG_FILE_NAME_CHARS: usize = 240;

    let now = Local::now();
    let mut file_name = render_log_path_component(template, context, &now)?;
    if file_name.is_empty()
        || matches!(file_name.as_str(), "." | "..")
        || file_name.ends_with(['.', ' '])
    {
        return Err(io::Error::other(
            "terminal session log template produced an invalid file name",
        ));
    }
    if !file_name.to_ascii_lowercase().ends_with(".log") {
        file_name.push_str(".log");
    }
    let stem = Path::new(&file_name)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or_default();
    if file_name.chars().count() > MAX_LOG_FILE_NAME_CHARS || is_windows_reserved_file_stem(stem) {
        return Err(io::Error::other(
            "terminal session log template produced an invalid file name",
        ));
    }
    Ok(file_name)
}

fn render_log_path_component(
    template: &ParsedTerminalSessionLogTemplate,
    context: &TerminalSessionLogContext,
    now: &DateTime<Local>,
) -> io::Result<String> {
    const MAX_LOG_PATH_COMPONENT_CHARS: usize = 240;

    let mut component = String::new();
    for part in template.parts() {
        match part {
            TerminalSessionLogTemplatePart::Literal(literal) => component.push_str(literal),
            TerminalSessionLogTemplatePart::Variable(variable) => component.push_str(
                &sanitize_file_name_component(&template_variable_value(*variable, context, now)),
            ),
        }
    }
    if component.is_empty()
        || matches!(component.as_str(), "." | "..")
        || component.ends_with(['.', ' '])
        || component.chars().count() > MAX_LOG_PATH_COMPONENT_CHARS
        || is_windows_reserved_file_stem(&component)
    {
        return Err(io::Error::other(
            "terminal session log template produced an invalid path component",
        ));
    }
    Ok(component)
}

fn unique_file_name(file_name: &str, suffix: usize) -> String {
    if suffix == 0 {
        return file_name.to_string();
    }
    let path = Path::new(file_name);
    let stem = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("log");
    match path.extension().and_then(|extension| extension.to_str()) {
        Some(extension) => format!("{stem}-{suffix}.{extension}"),
        None => format!("{stem}-{suffix}"),
    }
}

fn sanitize_file_name_component(value: &str) -> String {
    const MAX_COMPONENT_CHARS: usize = 64;

    let mut sanitized = String::new();
    let mut previous_was_separator = false;
    for character in value.chars().take(MAX_COMPONENT_CHARS) {
        let replace = character.is_control()
            || character.is_whitespace()
            || matches!(
                character,
                '/' | '\\' | '<' | '>' | ':' | '"' | '|' | '?' | '*'
            );
        if replace {
            if !previous_was_separator {
                sanitized.push('_');
            }
            previous_was_separator = true;
        } else {
            sanitized.push(character);
            previous_was_separator = false;
        }
    }
    let sanitized = sanitized.trim_matches(['.', '_']);
    if sanitized.is_empty() {
        "unknown".to_string()
    } else {
        sanitized.to_string()
    }
}

fn is_windows_reserved_file_stem(stem: &str) -> bool {
    let stem = stem
        .split('.')
        .next()
        .unwrap_or(stem)
        .trim_end_matches(['.', ' '])
        .to_ascii_uppercase();
    matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || stem
            .strip_prefix("COM")
            .or_else(|| stem.strip_prefix("LPT"))
            .is_some_and(|number| {
                matches!(number, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9")
            })
}

fn template_variable_value(
    variable: TerminalSessionLogTemplateVariable,
    context: &TerminalSessionLogContext,
    now: &DateTime<Local>,
) -> String {
    match variable {
        TerminalSessionLogTemplateVariable::Date => now.format("%Y-%m-%d").to_string(),
        TerminalSessionLogTemplateVariable::Time => now.format("%H-%M-%S").to_string(),
        TerminalSessionLogTemplateVariable::DateTime => {
            now.format("%Y-%m-%dT%H-%M-%S%:z").to_string()
        }
        TerminalSessionLogTemplateVariable::Timestamp => {
            now.format("%Y-%m-%d %H:%M:%S%.3f%:z").to_string()
        }
        TerminalSessionLogTemplateVariable::Session => context.session.clone(),
        TerminalSessionLogTemplateVariable::Host => context.host.clone(),
        TerminalSessionLogTemplateVariable::Username => context.username.clone(),
        TerminalSessionLogTemplateVariable::Protocol => context.protocol.clone(),
        TerminalSessionLogTemplateVariable::Text => String::new(),
    }
}

fn remove_expired_logs(directory: &Path, retention_days: u64) -> io::Result<()> {
    if retention_days == 0 {
        return Ok(());
    }
    let cutoff = SystemTime::now()
        .checked_sub(Duration::from_secs(
            retention_days.saturating_mul(24 * 60 * 60),
        ))
        .unwrap_or(SystemTime::UNIX_EPOCH);
    remove_expired_logs_before(directory, cutoff, 0)
}

fn remove_expired_logs_before(
    directory: &Path,
    cutoff: SystemTime,
    depth: usize,
) -> io::Result<()> {
    const MAX_SESSION_LOG_DIRECTORY_DEPTH: usize = 8;

    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.file_type().is_dir() && depth < MAX_SESSION_LOG_DIRECTORY_DEPTH {
            remove_expired_logs_before(&path, cutoff, depth + 1)?;
            continue;
        }
        if path.extension().and_then(|extension| extension.to_str()) != Some("log") {
            continue;
        }
        if !metadata.file_type().is_file() {
            continue;
        }
        if metadata.modified().is_ok_and(|modified| modified < cutoff) {
            fs::remove_file(path)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(directory: &Path) -> TerminalSessionLogOptions {
        TerminalSessionLogOptions {
            directory: directory.to_path_buf(),
            directory_template: String::new(),
            include_control_sequences: false,
            retention_days: 30,
            max_file_bytes: Some(1024),
            file_name_template: "{date}_{time}_{session}.log".to_string(),
            content_template: "{text}".to_string(),
            file_mode: TerminalSessionLogFileMode::Unique,
            context: TerminalSessionLogContext {
                session: "test".to_string(),
                host: "example.test".to_string(),
                username: "tester".to_string(),
                protocol: "ssh".to_string(),
            },
        }
    }

    #[test]
    fn printable_log_strips_split_ansi_sequences_without_losing_text() {
        let directory = tempfile::tempdir().unwrap();
        let mut log = TerminalSessionLog::start(options(directory.path())).unwrap();

        log.write_output(b"plain \x1b[3".to_vec()).unwrap();
        log.write_output("1m红色\x1b[0m\r\nnext".as_bytes().to_vec())
            .unwrap();
        let path = log.finish().unwrap();

        assert_eq!(fs::read_to_string(path).unwrap(), "plain 红色\r\nnext");
    }

    #[test]
    fn small_output_reaches_the_file_while_logging_is_still_active() {
        let directory = tempfile::tempdir().unwrap();
        let mut log = TerminalSessionLog::start(options(directory.path())).unwrap();
        let path = log.status().path.unwrap();
        log.write_output(b"streamed without a newline".to_vec())
            .unwrap();

        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while fs::read(&path).unwrap() != b"streamed without a newline"
            && std::time::Instant::now() < deadline
        {
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(fs::read(&path).unwrap(), b"streamed without a newline");
        log.finish().unwrap();
    }

    #[test]
    fn chosen_file_ignores_automatic_paths_and_retention_but_keeps_content_formatting() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("chosen {date}.txt");
        let unrelated = directory.path().join("old.log");
        fs::write(&path, b"replace me").unwrap();
        fs::write(&unrelated, b"keep me").unwrap();
        File::options()
            .write(true)
            .open(&unrelated)
            .unwrap()
            .set_times(
                std::fs::FileTimes::new()
                    .set_modified(SystemTime::now() - Duration::from_secs(2 * 24 * 60 * 60)),
            )
            .unwrap();
        let mut configured = options(&directory.path().join("automatic"));
        configured.directory_template = "{session}/{date}".into();
        configured.content_template = "{protocol}:{text}".into();
        configured.file_mode = TerminalSessionLogFileMode::Append;
        configured.retention_days = 1;

        let mut log = TerminalSessionLog::start_at_path(path.clone(), configured).unwrap();
        log.write_output(b"\x1b[31mchosen\x1b[0m\n".to_vec())
            .unwrap();
        assert_eq!(log.finish().unwrap(), path);
        assert_eq!(fs::read(&path).unwrap(), b"ssh:chosen\n");
        assert_eq!(fs::read(unrelated).unwrap(), b"keep me");
        assert!(!directory.path().join("automatic").exists());
    }

    #[test]
    fn invalid_content_template_does_not_truncate_the_chosen_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("chosen.txt");
        fs::write(&path, b"keep me").unwrap();
        let mut configured = options(directory.path());
        configured.content_template = "{unknown}".into();

        assert!(TerminalSessionLog::start_at_path(path.clone(), configured).is_err());
        assert_eq!(fs::read(path).unwrap(), b"keep me");
    }

    #[test]
    fn dropping_the_log_drains_accepted_output_and_finishes_the_last_line() {
        let directory = tempfile::tempdir().unwrap();
        let mut configured = options(directory.path());
        configured.max_file_bytes = None;
        configured.content_template = "{text} [saved]".into();
        let mut log = TerminalSessionLog::start(configured).unwrap();
        let path = log.status().path.unwrap();
        for _ in 0..128 {
            log.write_output(b"queued\n".to_vec()).unwrap();
        }
        log.write_output(b"tail".to_vec()).unwrap();
        drop(log);

        assert_eq!(
            fs::read_to_string(path).unwrap(),
            format!("{}tail [saved]", "queued [saved]\n".repeat(128))
        );
    }

    #[test]
    fn paused_log_skips_output_and_resumes_in_order() {
        let directory = tempfile::tempdir().unwrap();
        let mut log = TerminalSessionLog::start(options(directory.path())).unwrap();

        log.write_output(b"before\n".to_vec()).unwrap();
        log.pause().unwrap();
        log.write_output(b"secret\n".to_vec()).unwrap();
        log.resume();
        log.write_output(b"after\n".to_vec()).unwrap();
        let path = log.finish().unwrap();

        assert_eq!(fs::read_to_string(path).unwrap(), "before\nafter\n");
    }

    #[test]
    fn stalled_writer_accepts_small_fragments_and_rejects_over_budget_output_atomically() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("burst.log");
        let file = File::create(&path).unwrap();
        let (sender, receiver) = mpsc::sync_channel(1);
        let output = Arc::new(Mutex::new(SessionLogOutput::default()));
        let written = Arc::new(AtomicU64::new(0));
        let mut log = TerminalSessionLog {
            state: TerminalSessionLogState::Logging,
            path: path.clone(),
            sender: Some(sender),
            output: output.clone(),
            worker: None,
            bytes_written: written.clone(),
            failure: Arc::new(Mutex::new(SessionLogFailure::default())),
        };
        // No receiver runs until the entire burst has arrived, independent of thread scheduling.
        for _ in 0..10_000 {
            log.write_output(b"first\x1b[31m red\x1b[0m\r\n".to_vec())
                .unwrap();
        }
        let error = log
            .write_output(vec![b'x'; SESSION_LOG_BUFFER_BYTES])
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        log.write_output(b"last".to_vec()).unwrap();
        log.worker = Some(thread::spawn(move || {
            run_session_log_writer(
                file,
                receiver,
                output,
                written,
                false,
                None,
                parse_terminal_session_log_content_template("{text}").unwrap(),
                TerminalSessionLogContext::default(),
            )
        }));
        log.flush().unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            format!("{}last", "first red\r\n".repeat(10_000))
        );
        log.finish().unwrap();
    }

    #[test]
    fn log_size_failure_wakes_the_owner_without_more_output_even_when_attached_late() {
        for attach_late in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let mut bounded = options(directory.path());
            bounded.include_control_sequences = true;
            bounded.max_file_bytes = Some(4);
            let mut log = TerminalSessionLog::start(bounded).unwrap();
            let path = log.status().path.unwrap();
            let (wakeup, receiver) = async_channel::bounded(1);
            if !attach_late {
                log.wake_on_failure(wakeup.clone());
            }
            log.write_output(b"abcdef".to_vec()).unwrap();

            let deadline = Instant::now() + Duration::from_secs(3);
            while !log.has_failed() && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(1));
            }
            if attach_late {
                log.wake_on_failure(wakeup);
            }
            assert_eq!(receiver.try_recv(), Ok(()), "attach_late={attach_late}");
            assert_eq!(log.status().state, TerminalSessionLogState::Idle);
            assert!(log.finish().is_err());
            assert_eq!(fs::read(path).unwrap(), b"");
        }
    }

    #[test]
    fn unlimited_log_writes_past_the_default_test_boundary() {
        let directory = tempfile::tempdir().unwrap();
        let mut unlimited = options(directory.path());
        unlimited.include_control_sequences = true;
        unlimited.max_file_bytes = None;
        let mut log = TerminalSessionLog::start(unlimited).unwrap();
        let content = vec![b'x'; 2048];

        log.write_output(content.clone()).unwrap();
        let path = log.finish().unwrap();

        assert_eq!(fs::read(path).unwrap(), content);
    }

    #[test]
    fn starting_log_removes_only_expired_log_files() {
        let directory = tempfile::tempdir().unwrap();
        let expired = directory.path().join("expired.log");
        let nested_directory = directory.path().join("session");
        let nested_expired = nested_directory.join("expired.log");
        let unrelated = directory.path().join("notes.txt");
        fs::create_dir(&nested_directory).unwrap();
        fs::write(&expired, b"old").unwrap();
        fs::write(&nested_expired, b"old").unwrap();
        fs::write(&unrelated, b"keep").unwrap();
        let old_time = SystemTime::now() - Duration::from_secs(2 * 24 * 60 * 60);
        File::options()
            .write(true)
            .open(&expired)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(old_time))
            .unwrap();
        File::options()
            .write(true)
            .open(&nested_expired)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(old_time))
            .unwrap();

        let mut retention_options = options(directory.path());
        retention_options.retention_days = 1;
        let log = TerminalSessionLog::start(retention_options).unwrap();
        log.finish().unwrap();

        assert!(!expired.exists());
        assert!(!nested_expired.exists());
        assert!(unrelated.exists());
    }

    #[test]
    fn directory_template_creates_nested_session_log_path() {
        let directory = tempfile::tempdir().unwrap();
        let mut configured = options(directory.path());
        configured.directory_template = "{session}/{date}".to_string();

        let log = TerminalSessionLog::start(configured).unwrap();
        let path = log.status().path.unwrap();
        log.finish().unwrap();

        assert_eq!(
            path.parent().and_then(Path::parent).and_then(Path::parent),
            Some(directory.path())
        );
        assert_eq!(
            path.parent()
                .and_then(Path::parent)
                .and_then(Path::file_name)
                .and_then(|name| name.to_str()),
            Some("test")
        );
    }

    #[cfg(unix)]
    #[test]
    fn session_log_file_is_private_to_the_current_user() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let log = TerminalSessionLog::start(options(directory.path())).unwrap();
        let path = log.status().path.unwrap();
        log.finish().unwrap();

        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn content_template_formats_complete_and_split_lines() {
        let directory = tempfile::tempdir().unwrap();
        let mut configured = options(directory.path());
        configured.content_template = "{protocol}:{text} [{session}]".to_string();
        let mut log = TerminalSessionLog::start(configured).unwrap();

        log.write_output(b"first\r".to_vec()).unwrap();
        log.write_output(b"\nsec".to_vec()).unwrap();
        log.write_output(b"ond".to_vec()).unwrap();
        let path = log.finish().unwrap();

        assert_eq!(
            fs::read_to_string(path).unwrap(),
            "ssh:first [test]\r\nssh:second [test]"
        );
    }

    #[test]
    fn content_template_preserves_carriage_return_semantics_across_chunks() {
        for (input, expected) in [
            (
                "first\r\r\nsecond\r\n",
                "serial:first [test]\r\nserial:second [test]\r\n",
            ),
            ("first\r\r\r\n", "serial:first [test]\r\n"),
            (
                "first\r\n\r\nsecond",
                "serial:first [test]\r\nserial: [test]\r\nserial:second [test]",
            ),
            ("old\rnew\r\n", "serial:old\rnew [test]\r\n"),
            ("first\r\r", "serial:first [test]\r"),
            ("first\nsecond", "serial:first [test]\nserial:second [test]"),
        ] {
            for split in 0..=input.len() {
                let directory = tempfile::tempdir().unwrap();
                let mut configured = options(directory.path());
                configured.context.protocol = "serial".to_string();
                configured.content_template = "{protocol}:{text} [{session}]".to_string();
                let mut log = TerminalSessionLog::start(configured).unwrap();

                log.write_output(input.as_bytes()[..split].to_vec())
                    .unwrap();
                // A control-only chunk leaves no printable bytes and must not resolve a pending CR.
                log.write_output(b"\x1b[0m".to_vec()).unwrap();
                log.write_output(input.as_bytes()[split..].to_vec())
                    .unwrap();
                let path = log.finish().unwrap();

                assert_eq!(
                    fs::read(path).unwrap(),
                    expected.as_bytes(),
                    "input {input:?}, split {split}"
                );
            }
        }
    }

    #[test]
    fn append_and_overwrite_modes_use_the_rendered_file_name() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("ssh_test.log");
        fs::write(&path, b"existing\n").unwrap();
        let mut configured = options(directory.path());
        configured.file_name_template = "{protocol}_{session}.log".to_string();
        configured.file_mode = TerminalSessionLogFileMode::Append;
        let mut append = TerminalSessionLog::start(configured.clone()).unwrap();
        append.write_output(b"appended\n".to_vec()).unwrap();
        append.finish().unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "existing\nappended\n");

        configured.file_mode = TerminalSessionLogFileMode::Overwrite;
        let mut overwrite = TerminalSessionLog::start(configured).unwrap();
        overwrite.write_output(b"replacement\n".to_vec()).unwrap();
        overwrite.finish().unwrap();
        assert_eq!(fs::read_to_string(path).unwrap(), "replacement\n");
    }

    #[test]
    fn file_name_variables_cannot_escape_the_log_directory() {
        let directory = tempfile::tempdir().unwrap();
        let mut configured = options(directory.path());
        configured.file_name_template = "{session}.log".to_string();
        configured.context.session = "../../production host".to_string();

        let log = TerminalSessionLog::start(configured).unwrap();
        let path = log.status().path.unwrap();
        log.finish().unwrap();

        assert_eq!(path.parent(), Some(directory.path()));
        assert_eq!(
            path.file_name().and_then(|name| name.to_str()),
            Some("production_host.log")
        );
    }

    #[test]
    fn file_name_rejects_windows_device_names_on_every_platform() {
        let directory = tempfile::tempdir().unwrap();
        let mut configured = options(directory.path());
        configured.file_name_template = "CON.extra".to_string();

        assert!(TerminalSessionLog::start(configured).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn overwrite_mode_rejects_symbolic_link_targets() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let protected = directory.path().join("protected.txt");
        let link = directory.path().join("ssh_test.log");
        fs::write(&protected, b"keep").unwrap();
        symlink(&protected, &link).unwrap();
        let mut configured = options(directory.path());
        configured.file_name_template = "{protocol}_{session}.log".to_string();
        configured.file_mode = TerminalSessionLogFileMode::Overwrite;

        assert!(TerminalSessionLog::start(configured.clone()).is_err());
        assert!(TerminalSessionLog::start_at_path(link, configured).is_err());
        assert_eq!(fs::read(&protected).unwrap(), b"keep");

        let outside = tempfile::tempdir().unwrap();
        symlink(outside.path(), directory.path().join("test")).unwrap();
        let mut nested = options(directory.path());
        nested.directory_template = "{session}".to_string();
        assert!(TerminalSessionLog::start(nested).is_err());
    }
}
