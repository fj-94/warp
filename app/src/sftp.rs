//! Native SFTP support backed by the system OpenSSH client.

use std::{
    collections::BTreeSet,
    io,
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::{Duration, SystemTime},
};

use async_channel::Sender;
use bytes::BytesMut;
use futures_util::StreamExt;
use openssh_sftp_client::{Sftp, SftpOptions};
use parking_lot::Mutex;
use ssh2_config::{ParseRule, SshConfig};
use tokio::{
    io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt},
    process::{Child, Command},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const CONNECTION_TIMEOUT: Duration = Duration::from_secs(8);
const TRANSFER_BUFFER_SIZE: usize = 128 * 1024;
const SSH_ERROR_BUFFER_SIZE: usize = 8 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum SftpError {
    #[error("SSH target is invalid")]
    InvalidTarget,
    #[error("Could not start the system OpenSSH client: {0}")]
    SshUnavailable(#[source] io::Error),
    #[error("The SSH connection timed out")]
    ConnectionTimeout,
    #[error(
        "Could not establish an SFTP session: {0}. Check the host key and key-based authentication in a terminal first"
    )]
    Connection(String),
    #[error("SFTP operation failed: {0}")]
    Protocol(String),
    #[error("Local file operation failed: {0}")]
    LocalIo(#[from] io::Error),
    #[error("Destination already exists: {0}")]
    DestinationExists(String),
    #[error("Transfer cancelled")]
    Cancelled,
    #[error("The selected item is not a regular file")]
    UnsupportedFileType,
}

pub type Result<T> = std::result::Result<T, SftpError>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RemoteFileType {
    Directory,
    File,
    Symlink,
    Other,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RemoteEntry {
    pub name: String,
    pub path: String,
    pub file_type: RemoteFileType,
    pub size: Option<u64>,
    pub modified: Option<SystemTime>,
}

#[derive(Clone, Debug)]
pub struct TransferProgress {
    pub transferred: u64,
    pub total: Option<u64>,
}

struct ConnectionInner {
    target: String,
    sftp: Sftp,
    child: Mutex<Option<Child>>,
    stderr_task: Mutex<Option<JoinHandle<()>>>,
    stderr_buffer: Arc<Mutex<Vec<u8>>>,
}

impl Drop for ConnectionInner {
    fn drop(&mut self) {
        kill_child(&self.child);
        abort_stderr_task(&self.stderr_task);
    }
}

#[derive(Clone)]
pub struct SftpConnection {
    inner: Arc<ConnectionInner>,
}

impl std::fmt::Debug for SftpConnection {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SftpConnection")
            .field("target", &self.inner.target)
            .finish_non_exhaustive()
    }
}

impl SftpConnection {
    pub async fn connect(
        target: impl Into<String>,
        cancellation: CancellationToken,
    ) -> Result<Self> {
        let target = validate_target(&target.into())?;
        let command = ssh_command(&target);
        Self::connect_with_command(target, command, cancellation).await
    }

    async fn connect_with_command(
        target: String,
        mut command: Command,
        cancellation: CancellationToken,
    ) -> Result<Self> {
        let mut child = command.spawn().map_err(SftpError::SshUnavailable)?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| SftpError::Connection("ssh stdin was unavailable".to_string()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| SftpError::Connection("ssh stdout was unavailable".to_string()))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| SftpError::Connection("ssh stderr was unavailable".to_string()))?;
        let stderr_buffer = Arc::new(Mutex::new(Vec::new()));
        let stderr_task = tokio::spawn(drain_ssh_stderr(stderr, stderr_buffer.clone()));
        let child = Mutex::new(Some(child));

        let result = tokio::select! {
            _ = cancellation.cancelled() => {
                kill_child(&child);
                wait_for_stderr(&stderr_task).await;
                return Err(SftpError::Cancelled);
            }
            result = tokio::time::timeout(
                CONNECTION_TIMEOUT,
                Sftp::new(stdin, stdout, SftpOptions::new()),
            ) => result,
        };

        let sftp = match result {
            Ok(Ok(sftp)) => sftp,
            Ok(Err(error)) => {
                kill_child(&child);
                wait_for_stderr(&stderr_task).await;
                return Err(SftpError::Connection(connection_error_message(
                    &error.to_string(),
                    &stderr_buffer,
                )));
            }
            Err(_) => {
                kill_child(&child);
                wait_for_stderr(&stderr_task).await;
                let stderr = String::from_utf8_lossy(&stderr_buffer.lock())
                    .trim()
                    .to_string();
                if stderr.is_empty() {
                    return Err(SftpError::ConnectionTimeout);
                }
                return Err(SftpError::Connection(format!(
                    "connection timed out: {stderr}"
                )));
            }
        };

        Ok(Self {
            inner: Arc::new(ConnectionInner {
                target,
                sftp,
                child,
                stderr_task: Mutex::new(Some(stderr_task)),
                stderr_buffer,
            }),
        })
    }

    pub fn close(&self) {
        kill_child(&self.inner.child);
        abort_stderr_task(&self.inner.stderr_task);
    }

    pub async fn initial_directory(&self) -> Result<String> {
        let mut fs = self.inner.sftp.fs();
        let path = fs
            .canonicalize(Path::new("."))
            .await
            .map_err(|error| self.protocol_error(error))?;
        Ok(normalize_remote_path(&path.to_string_lossy()))
    }

    pub async fn canonicalize(&self, path: &str) -> Result<String> {
        let mut fs = self.inner.sftp.fs();
        let path = fs
            .canonicalize(Path::new(path))
            .await
            .map_err(|error| self.protocol_error(error))?;
        Ok(normalize_remote_path(&path.to_string_lossy()))
    }

    pub async fn list_dir(&self, path: &str) -> Result<Vec<RemoteEntry>> {
        let mut fs = self.inner.sftp.fs();
        let directory = fs
            .open_dir(Path::new(path))
            .await
            .map_err(|error| self.protocol_error(error))?;
        let reader = directory.read_dir();
        futures_util::pin_mut!(reader);
        let mut entries = Vec::new();

        while let Some(entry) = reader.next().await {
            let entry = entry.map_err(|error| self.protocol_error(error))?;
            let name = entry.filename().to_string_lossy().into_owned();
            if name == "." || name == ".." {
                continue;
            }
            let mut metadata = entry.metadata();
            let mut file_type = remote_file_type(entry.file_type());
            // Some SFTP servers omit the type bits in READDIR responses. In that
            // case the entry is still a real directory/file, so ask for its
            // attributes explicitly instead of rendering every entry as a file.
            if file_type == RemoteFileType::Other {
                if let Ok(stat) = fs
                    .symlink_metadata(Path::new(&join_remote_path(path, &name)))
                    .await
                {
                    metadata = stat;
                    file_type = remote_file_type(metadata.file_type());
                }
            }
            entries.push(RemoteEntry {
                path: join_remote_path(path, &name),
                name,
                file_type,
                size: metadata.len(),
                modified: metadata
                    .modified()
                    .map(|timestamp| timestamp.as_system_time()),
            });
        }

        entries.sort_by(|left, right| {
            remote_type_order(left.file_type)
                .cmp(&remote_type_order(right.file_type))
                .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
        });
        Ok(entries)
    }

    pub async fn rename(&self, from: &str, to: &str) -> Result<()> {
        let mut fs = self.inner.sftp.fs();
        fs.rename(Path::new(from), Path::new(to))
            .await
            .map_err(|error| self.protocol_error(error))
    }

    pub async fn create_directory(&self, path: &str) -> Result<()> {
        let mut fs = self.inner.sftp.fs();
        fs.create_dir(Path::new(path))
            .await
            .map_err(|error| self.protocol_error(error))
    }

    pub async fn create_file(&self, path: &str) -> Result<()> {
        let mut options = self.inner.sftp.options();
        options.write(true).create_new(true);
        options
            .open(Path::new(path))
            .await
            .map(|_| ())
            .map_err(|error| self.protocol_error(error))
    }

    pub async fn delete(&self, path: &str, file_type: RemoteFileType) -> Result<()> {
        let mut pending = vec![(path.to_string(), file_type, false)];
        let mut fs = self.inner.sftp.fs();

        while let Some((path, file_type, listed)) = pending.pop() {
            if file_type == RemoteFileType::Directory && !listed {
                let directory = fs
                    .open_dir(Path::new(&path))
                    .await
                    .map_err(|error| self.protocol_error(error))?;
                let reader = directory.read_dir();
                futures_util::pin_mut!(reader);
                let directory_path = path.clone();
                pending.push((path, RemoteFileType::Directory, true));
                while let Some(entry) = reader.next().await {
                    let entry = entry.map_err(|error| self.protocol_error(error))?;
                    let name = entry.filename().to_string_lossy().into_owned();
                    if name == "." || name == ".." {
                        continue;
                    }
                    let child_path = join_remote_path(&directory_path, &name);
                    let mut child_type = remote_file_type(entry.file_type());
                    if child_type == RemoteFileType::Other {
                        if let Ok(metadata) = fs.symlink_metadata(Path::new(&child_path)).await {
                            child_type = remote_file_type(metadata.file_type());
                        }
                    }
                    pending.push((child_path, child_type, false));
                }
                continue;
            }

            let result = if file_type == RemoteFileType::Directory {
                fs.remove_dir(Path::new(&path)).await
            } else {
                fs.remove_file(Path::new(&path)).await
            };
            result.map_err(|error| self.protocol_error(error))?;
        }

        Ok(())
    }

    pub async fn upload_file(
        &self,
        local_path: &Path,
        remote_directory: &str,
        overwrite: bool,
        cancellation: CancellationToken,
        progress: Sender<TransferProgress>,
    ) -> Result<()> {
        let file_name = local_path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or(SftpError::UnsupportedFileType)?;
        let destination = join_remote_path(remote_directory, file_name);
        if !overwrite && self.remote_exists(&destination).await? {
            return Err(SftpError::DestinationExists(destination));
        }

        let metadata = tokio::fs::metadata(local_path).await?;
        if !metadata.is_file() {
            return Err(SftpError::UnsupportedFileType);
        }
        let total = metadata.len();
        let temporary = temporary_remote_path(&destination);
        let result = self
            .upload_to_temporary(local_path, &temporary, total, cancellation, progress)
            .await;
        if let Err(error) = result {
            self.remove_remote_file_if_present(&temporary).await;
            return Err(error);
        }

        if !overwrite && self.remote_exists(&destination).await? {
            self.remove_remote_file_if_present(&temporary).await;
            return Err(SftpError::DestinationExists(destination));
        }

        let mut fs = self.inner.sftp.fs();
        if let Err(error) = fs
            .rename(Path::new(&temporary), Path::new(&destination))
            .await
        {
            if overwrite {
                if let Err(remove_error) = fs.remove_file(Path::new(&destination)).await {
                    self.remove_remote_file_if_present(&temporary).await;
                    return Err(self.protocol_error(remove_error));
                }
                if let Err(rename_error) = fs
                    .rename(Path::new(&temporary), Path::new(&destination))
                    .await
                {
                    self.remove_remote_file_if_present(&temporary).await;
                    return Err(self.protocol_error(rename_error));
                }
            } else {
                self.remove_remote_file_if_present(&temporary).await;
                return Err(self.protocol_error(error));
            }
        }
        Ok(())
    }

    async fn upload_to_temporary(
        &self,
        local_path: &Path,
        temporary: &str,
        total: u64,
        cancellation: CancellationToken,
        progress: Sender<TransferProgress>,
    ) -> Result<()> {
        let mut local = tokio::fs::File::open(local_path).await?;
        let mut remote = self
            .inner
            .sftp
            .create(Path::new(temporary))
            .await
            .map_err(|error| self.protocol_error(error))?;
        let mut buffer = vec![0; TRANSFER_BUFFER_SIZE];
        let mut transferred = 0;

        loop {
            let read = tokio::select! {
                _ = cancellation.cancelled() => return Err(SftpError::Cancelled),
                result = local.read(&mut buffer) => result?,
            };
            if read == 0 {
                break;
            }
            tokio::select! {
                _ = cancellation.cancelled() => return Err(SftpError::Cancelled),
                result = remote.write_all(&buffer[..read]) => result.map_err(|error| self.protocol_error(error))?,
            }
            transferred += read as u64;
            let _ = progress
                .send(TransferProgress {
                    transferred,
                    total: Some(total),
                })
                .await;
        }

        remote
            .close()
            .await
            .map_err(|error| self.protocol_error(error))
    }

    pub async fn download_file(
        &self,
        remote_path: &str,
        local_directory: &Path,
        overwrite: bool,
        cancellation: CancellationToken,
        progress: Sender<TransferProgress>,
    ) -> Result<()> {
        let file_name = remote_file_name(remote_path).ok_or(SftpError::UnsupportedFileType)?;
        let destination = local_directory.join(file_name);
        if !overwrite && tokio::fs::try_exists(&destination).await? {
            return Err(SftpError::DestinationExists(
                destination.display().to_string(),
            ));
        }

        let mut fs = self.inner.sftp.fs();
        let metadata = fs
            .metadata(Path::new(remote_path))
            .await
            .map_err(|error| self.protocol_error(error))?;
        if !metadata.file_type().is_some_and(|kind| kind.is_file()) {
            return Err(SftpError::UnsupportedFileType);
        }

        let temporary = temporary_local_path(&destination);
        let result = self
            .download_to_temporary(
                remote_path,
                &temporary,
                metadata.len(),
                cancellation,
                progress,
            )
            .await;
        if let Err(error) = result {
            let _ = tokio::fs::remove_file(&temporary).await;
            return Err(error);
        }

        if !overwrite && tokio::fs::try_exists(&destination).await? {
            let _ = tokio::fs::remove_file(&temporary).await;
            return Err(SftpError::DestinationExists(
                destination.display().to_string(),
            ));
        }

        if overwrite && tokio::fs::try_exists(&destination).await? {
            if let Err(error) = tokio::fs::remove_file(&destination).await {
                let _ = tokio::fs::remove_file(&temporary).await;
                return Err(SftpError::LocalIo(error));
            }
        }
        if let Err(error) = tokio::fs::rename(&temporary, &destination).await {
            let _ = tokio::fs::remove_file(&temporary).await;
            return Err(SftpError::LocalIo(error));
        }
        Ok(())
    }

    async fn download_to_temporary(
        &self,
        remote_path: &str,
        temporary: &Path,
        total: Option<u64>,
        cancellation: CancellationToken,
        progress: Sender<TransferProgress>,
    ) -> Result<()> {
        let mut remote = self
            .inner
            .sftp
            .open(Path::new(remote_path))
            .await
            .map_err(|error| self.protocol_error(error))?;
        let mut local = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(temporary)
            .await?;
        let mut buffer = BytesMut::with_capacity(TRANSFER_BUFFER_SIZE);
        let mut transferred = 0;

        loop {
            let read_buffer = tokio::select! {
                _ = cancellation.cancelled() => return Err(SftpError::Cancelled),
                result = remote.read(TRANSFER_BUFFER_SIZE as u32, buffer) => {
                    result.map_err(|error| self.protocol_error(error))?
                },
            };
            let Some(mut read_buffer) = read_buffer else {
                break;
            };
            if read_buffer.is_empty() {
                break;
            }
            let next_offset = transferred + read_buffer.len() as u64;
            remote.seek(io::SeekFrom::Start(next_offset)).await?;
            tokio::select! {
                _ = cancellation.cancelled() => return Err(SftpError::Cancelled),
                result = local.write_all(&read_buffer) => result?,
            }
            transferred = next_offset;
            let _ = progress.send(TransferProgress { transferred, total }).await;
            read_buffer.clear();
            buffer = read_buffer;
        }

        local.flush().await?;
        remote
            .close()
            .await
            .map_err(|error| self.protocol_error(error))
    }

    async fn remote_exists(&self, path: &str) -> Result<bool> {
        let mut fs = self.inner.sftp.fs();
        match fs.metadata(Path::new(path)).await {
            Ok(_) => Ok(true),
            Err(openssh_sftp_client::Error::SftpError(
                openssh_sftp_client::error::SftpErrorKind::NoSuchFile,
                _,
            )) => Ok(false),
            Err(error) => Err(self.protocol_error(error)),
        }
    }

    async fn remove_remote_file_if_present(&self, path: &str) {
        let mut fs = self.inner.sftp.fs();
        let _ = fs.remove_file(Path::new(path)).await;
    }

    fn protocol_error(&self, error: openssh_sftp_client::Error) -> SftpError {
        let mut message = error.to_string();
        let stderr = String::from_utf8_lossy(&self.inner.stderr_buffer.lock())
            .trim()
            .to_string();
        if !stderr.is_empty() {
            message.push_str(": ");
            message.push_str(&stderr);
        }

        if let Some(child) = self.inner.child.lock().as_mut() {
            match child.try_wait() {
                Ok(Some(status)) => message.push_str(&format!(" (ssh exited with {status})")),
                Ok(None) => {}
                Err(error) => message.push_str(&format!(" (could not read ssh status: {error})")),
            }
        }
        SftpError::Protocol(message)
    }

    pub fn is_transport_failure(&self, error: &SftpError) -> bool {
        matches!(error, SftpError::Protocol(message) if message.contains("read/flush task failed")
            || message.contains("ssh exited with"))
    }
}

pub fn load_ssh_host_aliases() -> Vec<String> {
    let Ok(config) = SshConfig::parse_default_file(ParseRule::STRICT) else {
        return vec![];
    };
    aliases_from_config(&config)
}

fn aliases_from_config(config: &SshConfig) -> Vec<String> {
    config
        .get_hosts()
        .iter()
        .flat_map(|host| host.pattern.iter())
        .filter(|clause| {
            !clause.negated
                && !clause.pattern.contains('*')
                && !clause.pattern.contains('?')
                && validate_target(&clause.pattern).is_ok()
        })
        .map(|clause| clause.pattern.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn validate_target(target: &str) -> Result<String> {
    let trimmed = target.trim();
    if trimmed.is_empty()
        || trimmed != target
        || trimmed.starts_with('-')
        || trimmed.chars().any(char::is_whitespace)
        || trimmed.chars().any(char::is_control)
    {
        return Err(SftpError::InvalidTarget);
    }

    let Some((destination, _port)) = split_target_port(trimmed) else {
        return Err(SftpError::InvalidTarget);
    };
    let host = destination
        .rsplit_once('@')
        .map_or(destination, |(_, host)| host);
    let host = host
        .strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or(host);
    let valid_host = !host.is_empty()
        && host.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b':' | b'%')
        });
    let valid_user = destination.rsplit_once('@').map_or(true, |(user, _)| {
        !user.is_empty()
            && user
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    });
    if !valid_host || !valid_user || (trimmed.matches('@').count() > 1) {
        return Err(SftpError::InvalidTarget);
    }
    Ok(trimmed.to_string())
}

fn ssh_command(target: &str) -> Command {
    let executable = if cfg!(windows) { "ssh.exe" } else { "ssh" };
    let mut command = Command::new(executable);
    let (destination, port) = split_target_port(target).unwrap_or((target, None));
    command
        .arg("-o")
        .arg("BatchMode=yes")
        .arg("-o")
        .arg("ConnectTimeout=8")
        .arg("-o")
        .arg("ConnectionAttempts=1")
        .arg("-o")
        .arg("RequestTTY=no")
        .args(port.into_iter().flat_map(|port| ["-p", port]))
        .arg("-s")
        .arg(destination)
        .arg("sftp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    command
}

fn split_target_port(target: &str) -> Option<(&str, Option<&str>)> {
    let host_start = target.rfind('@').map_or(0, |index| index + 1);
    let host = &target[host_start..];
    if host.starts_with('[') {
        let closing = host.find(']')?;
        let suffix = &host[closing + 1..];
        return if suffix.is_empty() {
            Some((target, None))
        } else if let Some(port) = suffix.strip_prefix(':') {
            (!port.is_empty() && port.bytes().all(|byte| byte.is_ascii_digit()))
                .then(|| (&target[..target.len() - port.len() - 1], Some(port)))
        } else {
            None
        };
    }

    if host.matches(':').count() == 1 {
        let (host, port) = host.rsplit_once(':')?;
        if host.is_empty() || port.is_empty() || !port.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        Some((&target[..target.len() - port.len() - 1], Some(port)))
    } else {
        Some((target, None))
    }
}

fn kill_child(child: &Mutex<Option<Child>>) {
    if let Some(mut child) = child.lock().take() {
        let _ = child.start_kill();
    }
}

fn abort_stderr_task(task: &Mutex<Option<JoinHandle<()>>>) {
    if let Some(task) = task.lock().take() {
        task.abort();
    }
}

async fn drain_ssh_stderr(mut stderr: tokio::process::ChildStderr, buffer: Arc<Mutex<Vec<u8>>>) {
    let mut chunk = [0; 1024];
    loop {
        let Ok(read) = stderr.read(&mut chunk).await else {
            break;
        };
        if read == 0 {
            break;
        }

        let mut buffer = buffer.lock();
        buffer.extend_from_slice(&chunk[..read]);
        if buffer.len() > SSH_ERROR_BUFFER_SIZE {
            let overflow = buffer.len() - SSH_ERROR_BUFFER_SIZE;
            buffer.drain(..overflow);
        }
    }
}

async fn wait_for_stderr(task: &JoinHandle<()>) {
    let _ = tokio::time::timeout(Duration::from_millis(250), async {
        while !task.is_finished() {
            tokio::task::yield_now().await;
        }
    })
    .await;
}

fn connection_error_message(error: &str, stderr: &Mutex<Vec<u8>>) -> String {
    let stderr = String::from_utf8_lossy(&stderr.lock()).trim().to_string();
    if stderr.is_empty() {
        error.to_string()
    } else {
        format!("{error}: {stderr}")
    }
}

pub fn normalize_remote_path(path: &str) -> String {
    let path = path.replace('\\', "/");
    let absolute = path.starts_with('/');
    let normalized = path
        .split('/')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("/");
    if normalized.is_empty() {
        "/".to_string()
    } else if absolute {
        format!("/{normalized}")
    } else {
        normalized
    }
}

pub fn join_remote_path(directory: &str, name: &str) -> String {
    let directory = normalize_remote_path(directory);
    if directory == "/" {
        format!("/{name}")
    } else {
        format!("{directory}/{name}")
    }
}

pub fn parent_remote_path(path: &str) -> String {
    let path = normalize_remote_path(path);
    if path == "/" {
        return path;
    }
    path.rsplit_once('/')
        .map(|(parent, _)| {
            if parent.is_empty() {
                "/".to_string()
            } else {
                parent.to_string()
            }
        })
        .unwrap_or_else(|| "/".to_string())
}

fn remote_file_name(path: &str) -> Option<&str> {
    path.trim_end_matches('/')
        .rsplit('/')
        .next()
        .filter(|name| !name.is_empty())
}

fn temporary_remote_path(destination: &str) -> String {
    format!("{destination}.warp-part-{}", Uuid::new_v4())
}

fn temporary_local_path(destination: &Path) -> PathBuf {
    let file_name = destination
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("download");
    destination.with_file_name(format!(".{file_name}.warp-part-{}", Uuid::new_v4()))
}

fn remote_type_order(file_type: RemoteFileType) -> u8 {
    match file_type {
        RemoteFileType::Directory => 0,
        RemoteFileType::File => 1,
        RemoteFileType::Symlink => 2,
        RemoteFileType::Other => 3,
    }
}

fn remote_file_type(file_type: Option<openssh_sftp_client::metadata::FileType>) -> RemoteFileType {
    file_type.map_or(RemoteFileType::Other, |file_type| {
        if file_type.is_dir() {
            RemoteFileType::Directory
        } else if file_type.is_file() {
            RemoteFileType::File
        } else if file_type.is_symlink() {
            RemoteFileType::Symlink
        } else {
            RemoteFileType::Other
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ssh2_config::{Host, HostClause, HostParams};

    #[test]
    fn rejects_targets_that_can_be_interpreted_as_options() {
        for target in [
            "",
            " host",
            "host ",
            "-Fconfig",
            "host name",
            "host\nother",
            "ubuntu@ubuntu:welcome-\u{4e3b}\u{673a}",
            "user@@host",
            "host:not-a-port",
        ] {
            assert!(validate_target(target).is_err(), "accepted {target:?}");
        }
        for target in [
            "server",
            "user@server",
            "server.example:2222",
            "[::1]",
            "user@[::1]:2222",
        ] {
            assert_eq!(validate_target(target).unwrap(), target);
        }
    }

    #[test]
    fn invokes_openssh_sftp_subsystem_without_a_shell() {
        let command = ssh_command("user@example.com");
        let command = command.as_std();
        let expected_program = if cfg!(windows) { "ssh.exe" } else { "ssh" };
        assert_eq!(command.get_program(), expected_program);
        assert_eq!(
            command
                .get_args()
                .map(|argument| argument.to_string_lossy().into_owned())
                .collect::<Vec<_>>(),
            [
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=8",
                "-o",
                "ConnectionAttempts=1",
                "-o",
                "RequestTTY=no",
                "-s",
                "user@example.com",
                "sftp",
            ]
        );

        let command = ssh_command("user@example.com:2222");
        assert_eq!(
            command
                .as_std()
                .get_args()
                .map(|argument| argument.to_string_lossy().into_owned())
                .collect::<Vec<_>>(),
            [
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=8",
                "-o",
                "ConnectionAttempts=1",
                "-o",
                "RequestTTY=no",
                "-p",
                "2222",
                "-s",
                "user@example.com",
                "sftp",
            ]
        );
    }

    #[test]
    fn enumerates_only_explicit_positive_aliases() {
        let defaults = ssh2_config::DefaultAlgorithms::default();
        let config = SshConfig::from_hosts(vec![
            Host::new(
                vec![
                    HostClause::new("production".to_string(), false),
                    HostClause::new("*.internal".to_string(), false),
                    HostClause::new("excluded".to_string(), true),
                ],
                HostParams::new(&defaults),
            ),
            Host::new(
                vec![HostClause::new("staging".to_string(), false)],
                HostParams::new(&defaults),
            ),
        ]);

        assert_eq!(aliases_from_config(&config), vec!["production", "staging"]);
    }

    #[test]
    fn remote_paths_are_posix_style() {
        assert_eq!(normalize_remote_path(r"\\home\\warp\\"), "/home/warp");
        assert_eq!(join_remote_path("/", "file.txt"), "/file.txt");
        assert_eq!(
            join_remote_path("/home/warp/", "file.txt"),
            "/home/warp/file.txt"
        );
        assert_eq!(parent_remote_path("/home/warp"), "/home");
        assert_eq!(parent_remote_path("/home"), "/");
        assert_eq!(parent_remote_path("/"), "/");
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn downloads_from_a_real_sftp_server() {
        let server = Path::new("/usr/lib/openssh/sftp-server");
        if !server.exists() {
            return;
        }

        let root = std::env::temp_dir().join(format!("warp-sftp-test-{}", Uuid::new_v4()));
        let download_directory = root.join("download");
        tokio::fs::create_dir_all(&download_directory)
            .await
            .unwrap();
        let remote_path = root.join("remote.bin");
        let contents = (0..(TRANSFER_BUFFER_SIZE * 3 + 17))
            .map(|index| (index % 251) as u8)
            .collect::<Vec<_>>();
        tokio::fs::write(&remote_path, &contents).await.unwrap();

        let mut command = Command::new(server);
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let connection = SftpConnection::connect_with_command(
            "local-test".to_string(),
            command,
            CancellationToken::new(),
        )
        .await
        .unwrap();
        let (progress, _progress_rx) = async_channel::unbounded();

        connection
            .download_file(
                remote_path.to_str().unwrap(),
                &download_directory,
                false,
                CancellationToken::new(),
                progress,
            )
            .await
            .unwrap();

        let downloaded = tokio::fs::read(download_directory.join("remote.bin"))
            .await
            .unwrap();
        assert_eq!(downloaded, contents);
        connection.close();
        tokio::fs::remove_dir_all(root).await.unwrap();
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn manages_entries_on_a_real_sftp_server() {
        let server = Path::new("/usr/lib/openssh/sftp-server");
        if !server.exists() {
            return;
        }

        let root = std::env::temp_dir().join(format!("warp-sftp-test-{}", Uuid::new_v4()));
        let remote_directory = root.join("remote-dir");
        tokio::fs::create_dir_all(remote_directory.join("nested"))
            .await
            .unwrap();
        tokio::fs::write(remote_directory.join("nested/file.txt"), b"nested")
            .await
            .unwrap();
        let local_upload = root.join("upload.txt");
        tokio::fs::write(&local_upload, b"uploaded").await.unwrap();

        let mut command = Command::new(server);
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let connection = SftpConnection::connect_with_command(
            "local-test".to_string(),
            command,
            CancellationToken::new(),
        )
        .await
        .unwrap();

        let root_path = root.to_string_lossy();
        let entries = connection.list_dir(&root_path).await.unwrap();
        assert!(entries.iter().any(|entry| {
            entry.name == "remote-dir" && entry.file_type == RemoteFileType::Directory
        }));

        let remote_directory_path = remote_directory.to_string_lossy();
        let created_directory = remote_directory.join("created-directory");
        let created_file = remote_directory.join("empty.txt");
        connection
            .create_directory(&created_directory.to_string_lossy())
            .await
            .unwrap();
        connection
            .create_file(&created_file.to_string_lossy())
            .await
            .unwrap();
        let entries = connection.list_dir(&remote_directory_path).await.unwrap();
        assert!(entries.iter().any(|entry| {
            entry.name == "created-directory" && entry.file_type == RemoteFileType::Directory
        }));
        assert!(entries.iter().any(|entry| {
            entry.name == "empty.txt"
                && entry.file_type == RemoteFileType::File
                && entry.size == Some(0)
        }));

        let (progress, _progress_rx) = async_channel::unbounded();
        connection
            .upload_file(
                &local_upload,
                &remote_directory_path,
                false,
                CancellationToken::new(),
                progress,
            )
            .await
            .unwrap();
        assert_eq!(
            tokio::fs::read(remote_directory.join("upload.txt"))
                .await
                .unwrap(),
            b"uploaded"
        );
        tokio::fs::write(&local_upload, b"saved").await.unwrap();
        let (progress, _progress_rx) = async_channel::unbounded();
        connection
            .upload_file(
                &local_upload,
                &remote_directory_path,
                true,
                CancellationToken::new(),
                progress,
            )
            .await
            .unwrap();
        assert_eq!(
            tokio::fs::read(remote_directory.join("upload.txt"))
                .await
                .unwrap(),
            b"saved"
        );

        let renamed = remote_directory.join("renamed.txt");
        connection
            .rename(
                &remote_directory.join("upload.txt").to_string_lossy(),
                &renamed.to_string_lossy(),
            )
            .await
            .unwrap();
        assert!(tokio::fs::try_exists(&renamed).await.unwrap());

        connection
            .delete(&created_file.to_string_lossy(), RemoteFileType::File)
            .await
            .unwrap();
        connection
            .delete(
                &created_directory.to_string_lossy(),
                RemoteFileType::Directory,
            )
            .await
            .unwrap();
        assert!(!tokio::fs::try_exists(&created_file).await.unwrap());
        assert!(!tokio::fs::try_exists(&created_directory).await.unwrap());

        connection
            .delete(&remote_directory_path, RemoteFileType::Directory)
            .await
            .unwrap();
        assert!(!tokio::fs::try_exists(&remote_directory).await.unwrap());

        connection.close();
        tokio::fs::remove_dir_all(root).await.unwrap();
    }
}
