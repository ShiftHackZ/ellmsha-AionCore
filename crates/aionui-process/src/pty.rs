//! Owned PTY handle. No terminal output is ever mirrored to host logs.
use crate::ProcessError;
use aionui_runtime::Builder;
use portable_pty::{Child, MasterPty, PtySize};
use std::{
    io::{Read, Write},
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};

pub struct ManagedPty {
    pid: u32,
    master: Mutex<Option<Box<dyn MasterPty + Send>>>,
    writer: Mutex<Option<Box<dyn Write + Send>>>,
    child: Mutex<Option<Box<dyn Child + Send + Sync>>>,
    descendants: Mutex<Vec<(u32, Option<u64>)>>,
    /// Last observed exit code. The child remains exclusively owned here.
    exit: Mutex<Option<u32>>,
    teardown: tokio::sync::Mutex<()>,
    #[cfg(windows)]
    job: crate::process::job_windows::JobObject,
}
// portable-pty opens ConPTY with INHERIT_CURSOR. Until the host replies to
// its initial device status request, cmd can wait indefinitely on an empty
// screen. Do this before exposing the writer to command or interactive input.
#[cfg(windows)]
fn initialize_conpty(
    mut reader: Box<dyn Read + Send>,
    process: &ManagedPty,
) -> Result<Box<dyn Read + Send>, ProcessError> {
    let (send, receive) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || {
        const REQUEST: &[u8] = b"\x1b[6n";
        let mut startup = Vec::new();
        let mut chunk = [0; 512];
        while startup.len() < 8192 {
            let count = match reader.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            };
            let previous = startup.len();
            startup.extend_from_slice(&chunk[..count]);
            let start = previous.saturating_sub(REQUEST.len() - 1);
            if let Some(index) = startup[start..].windows(REQUEST.len()).position(|bytes| bytes == REQUEST) {
                startup.drain(start + index..start + index + REQUEST.len());
                let _ = send.send(std::io::Cursor::new(startup).chain(reader));
                return;
            }
        }
    });
    let reader = receive
        .recv_timeout(Duration::from_secs(10))
        .map_err(|_| ProcessError::internal("ConPTY cursor handshake failed"))?;
    process.write(b"\x1b[1;1R")?;
    Ok(Box::new(reader))
}

impl ManagedPty {
    /// Spawn a persistent interactive shell, or a single foreground command.
    /// On Windows cmd /D waits for input until its Job Object is assigned.
    pub fn spawn(cwd: &Path, command: &str) -> Result<(Arc<Self>, Box<dyn Read + Send>), ProcessError> {
        #[cfg(unix)]
        let mut builder = {
            let shell = if command.is_empty() {
                std::env::var_os("SHELL")
                    .filter(|s| Path::new(s).is_absolute() && Path::new(s).is_file())
                    .unwrap_or_else(|| "/bin/sh".into())
            } else {
                "/bin/sh".into()
            };
            let mut b = Builder::new(shell);
            if command.is_empty() {
                b.arg("-i");
            } else {
                b.args(["-c", command]);
            }
            b
        };
        #[cfg(windows)]
        let mut builder = {
            let mut b = Builder::new("cmd.exe");
            b.args(["/D", "/Q"]);
            b
        };
        builder
            .current_dir(cwd)
            .env("TERM", "xterm-256color")
            .env_remove("NO_COLOR");
        let (pair, mut child) = builder
            .spawn_pty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|_| ProcessError::internal("PTY spawn failed"))?;
        let prepared = (|| {
            let pid = child
                .process_id()
                .ok_or_else(|| ProcessError::internal("PTY child has no PID"))?;
            let reader = pair
                .master
                .try_clone_reader()
                .map_err(|_| ProcessError::internal("PTY reader unavailable"))?;
            let writer = pair
                .master
                .take_writer()
                .map_err(|_| ProcessError::internal("PTY writer unavailable"))?;
            Ok((pid, reader, writer))
        })();
        let (pid, reader, writer) = match prepared {
            Ok(value) => value,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error);
            }
        };
        #[cfg(windows)]
        let job = match child
            .as_raw_handle()
            .ok_or_else(|| "PTY handle unavailable".to_string())
            .and_then(crate::process::job_windows::JobObject::assign)
        {
            Ok(job) => job,
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(ProcessError::internal("PTY job assignment failed"));
            }
        };
        #[cfg(unix)]
        if let Some(fd) = pair.master.as_raw_fd() {
            // Bound backpressure: a raw-mode program may stop reading stdin.
            // The reader handles WouldBlock because duplicated PTY descriptors
            // share this status flag.
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
            if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
                let _ = child.kill();
                let _ = child.wait();
                return Err(ProcessError::internal("PTY nonblocking setup failed"));
            }
        }
        drop(pair.slave);
        let process = Arc::new(Self {
            pid,
            master: Mutex::new(Some(pair.master)),
            writer: Mutex::new(Some(writer)),
            child: Mutex::new(Some(child)),
            descendants: Mutex::new(Vec::new()),
            exit: Mutex::new(None),
            teardown: tokio::sync::Mutex::new(()),
            #[cfg(windows)]
            job,
        });
        #[cfg(windows)]
        let reader = initialize_conpty(reader, &process)?;
        #[cfg(windows)]
        if !command.is_empty() {
            process.write(format!("{command}\r\nexit\r\n").as_bytes())?;
        }
        let weak = Arc::downgrade(&process);
        tokio::spawn(async move {
            loop {
                let weak = weak.clone();
                let done = tokio::task::spawn_blocking(move || {
                    let Some(process) = weak.upgrade() else {
                        return true;
                    };
                    let mut tracked = process.descendants.lock().unwrap();
                    tracked.retain(|(pid, _)| !matches!(crate::probe(*pid), crate::ObservedLiveness::Gone));
                    if !process.has_exited() {
                        for identity in crate::proc_tree::escaped_descendants(process.pid, None) {
                            if tracked.len() < 4096 && !tracked.contains(&identity) {
                                tracked.push(identity);
                            }
                        }
                        false
                    } else {
                        crate::proc_tree::reap(&tracked);
                        true
                    }
                })
                .await
                .unwrap_or(true);
                if done {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        });
        Ok((process, reader))
    }
    pub fn pid(&self) -> u32 {
        self.pid
    }
    pub fn exit_code(&self) -> Option<i32> {
        self.exit.lock().unwrap().map(|code| code as i32)
    }
    pub fn has_exited(&self) -> bool {
        let mut child = self.child.lock().unwrap();
        let mut exit = self.exit.lock().unwrap();
        if exit.is_none()
            && let Some(child) = child.as_mut()
            && let Ok(Some(status)) = child.try_wait()
        {
            *exit = Some(status.exit_code());
        }
        exit.is_some()
    }
    pub fn write(&self, bytes: &[u8]) -> Result<(), ProcessError> {
        if self.has_exited() {
            return Err(ProcessError::internal("Terminal has exited"));
        }
        let mut writer = self.writer.lock().unwrap();
        let writer = writer
            .as_mut()
            .ok_or_else(|| ProcessError::internal("Terminal is closed"))?;
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let mut remaining = bytes;
        while !remaining.is_empty() {
            match writer.write(remaining) {
                Ok(0) => return Err(ProcessError::internal("PTY input closed")),
                Ok(count) => remaining = &remaining[count..],
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error)
                    if error.kind() == std::io::ErrorKind::WouldBlock && std::time::Instant::now() < deadline =>
                {
                    std::thread::sleep(Duration::from_millis(5))
                }
                Err(_) => return Err(ProcessError::internal("PTY input failed or timed out")),
            }
        }
        Ok(())
    }

    pub fn resize(&self, rows: u16, cols: u16) -> Result<(), ProcessError> {
        self.master
            .lock()
            .unwrap()
            .as_mut()
            .ok_or_else(|| ProcessError::internal("Terminal is closed"))?
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|_| ProcessError::internal("PTY resize failed"))
    }
    /// Capture descendants before signalling the shell, including job-control
    /// children in other process groups; identity-gated reap never targets peers.
    pub async fn terminate(self: &Arc<Self>) -> Result<(), ProcessError> {
        let _guard = self.teardown.lock().await;
        let already_exited = self.has_exited();
        let mut descendants = self.descendants.lock().unwrap().clone();
        if !already_exited {
            descendants.extend(crate::proc_tree::escaped_descendants(self.pid, None));
        }
        #[cfg(unix)]
        {
            if let Ok(mut writer) = self.writer.try_lock()
                && let Some(writer) = writer.as_mut()
            {
                let _ = writer.write(b"\x03");
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        let process = self.clone();
        tokio::task::spawn_blocking(move || {
            let mut owned = process.child.lock().unwrap();
            #[cfg(windows)]
            process.job.terminate();
            if let Some(child) = owned.as_mut()
                && process.exit.lock().unwrap().is_none()
            {
                match child.try_wait() {
                    Ok(Some(status)) => *process.exit.lock().unwrap() = Some(status.exit_code()),
                    Ok(None) => {
                        let _ = child.kill();
                    }
                    Err(_) => {}
                }
            }
        })
        .await
        .map_err(|_| ProcessError::internal("PTY cleanup failed"))?;
        let remaining = crate::proc_tree::reap(&descendants);
        tokio::time::timeout(Duration::from_secs(5), async {
            while !self.has_exited() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .map_err(|_| ProcessError::internal("PTY exit could not be confirmed"))?;
        self.writer.lock().unwrap().take();
        self.master.lock().unwrap().take();
        if remaining != 0 {
            return Err(ProcessError::internal("PTY descendant cleanup could not be confirmed"));
        }
        Ok(())
    }
}
impl Drop for ManagedPty {
    fn drop(&mut self) {
        let mut descendants = self.descendants.lock().unwrap().clone();
        if !self.has_exited() {
            descendants.extend(crate::proc_tree::escaped_descendants(self.pid, None));
        }
        #[cfg(windows)]
        self.job.terminate();
        if let Some(mut child) = self.child.lock().unwrap().take() {
            let exited = self.exit.lock().unwrap().is_some();
            // The child has a single owner/waiter. Moving it to a reaper avoids
            // portable-pty's bounded SIGHUP wait blocking an async worker.
            std::thread::spawn(move || {
                if !exited {
                    let _ = child.kill();
                }
                let _ = child.wait();
            });
        }
        crate::proc_tree::reap(&descendants);
    }
}
