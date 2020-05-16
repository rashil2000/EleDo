use crate::command::Command;
use crate::pipe::*;
use crate::process::Process;
use crate::psuedocon::PsuedoCon;
use crate::win32_error_with_context;
use crate::Token;
use std::ffi::OsString;
use std::io::{Error as IoError, Result as IoResult, Write};
use std::os::windows::prelude::*;
use std::path::{Path, PathBuf};
use winapi::shared::minwindef::DWORD;
use winapi::um::consoleapi::{GetConsoleMode, SetConsoleMode};
use winapi::um::fileapi::GetFileType;
use winapi::um::winbase::FILE_TYPE_CHAR;
use winapi::um::wincon::{
    GetConsoleScreenBufferInfo, CONSOLE_SCREEN_BUFFER_INFO, DISABLE_NEWLINE_AUTO_RETURN,
    ENABLE_PROCESSED_OUTPUT, ENABLE_VIRTUAL_TERMINAL_INPUT, ENABLE_VIRTUAL_TERMINAL_PROCESSING,
    ENABLE_WRAP_AT_EOL_OUTPUT,
};
use winapi::um::wincontypes::COORD;

pub struct BridgePtyClient {
    con: PsuedoCon,
}

impl BridgePtyClient {
    pub fn with_params(conin: &Path, conout: &Path, width: usize, height: usize) -> IoResult<Self> {
        let client_to_server = PipeHandle::open_pipe(conout)?;
        let server_to_client = PipeHandle::open_pipe(conin)?;

        let con = PsuedoCon::new(
            COORD {
                X: width as i16,
                Y: height as i16,
            },
            server_to_client,
            client_to_server,
        )?;

        Ok(Self { con })
    }

    pub fn spawn(&self, mut command: Command) -> IoResult<Process> {
        command.spawn_with_pty(&self.con)
    }

    pub fn run(self, proc: Process) -> IoResult<DWORD> {
        proc.wait_for(None)?;
        proc.exit_code()
    }
}

fn join_with_timeout(join_handle: std::thread::JoinHandle<()>, timeout: std::time::Duration) {
    use std::sync::mpsc::channel;
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        let _ = join_handle.join();
        let _ = tx.send(());
    });
    let _ = rx.recv_timeout(timeout);
}

/// The bridge server is the originator of the spawned command.
/// It owns the server end of the connection and awaits the
/// bridge client connection.
pub struct BridgeServer {
    stdin_is_pty: bool,
    stdout_is_pty: bool,
    stderr_is_pty: bool,

    stdin: Option<PipeHandle>,
    stdout: Option<PipeHandle>,
    stderr: Option<PipeHandle>,

    conin: Option<PipeHandle>,
    conin_pipe: Option<PipeHandle>,
    conout: Option<PipeHandle>,
    conout_pipe: Option<PipeHandle>,

    input_mode: Option<DWORD>,
    output_mode: Option<DWORD>,
}

impl Drop for BridgeServer {
    fn drop(&mut self) {
        if let Some(mode) = self.output_mode {
            if let Ok(mut conout) = PipeHandle::open_pipe("CONOUT$") {
                // Emit a soft reset
                let _ = write!(&mut conout, "\x1b[!p");
                // Restore mode
                let _ = set_console_mode(&conout, mode);
            }
        }
        if let Some(mode) = self.input_mode {
            if let Ok(conin) = PipeHandle::open_pipe("CONIN$") {
                let _ = set_console_mode(&conin, mode);
            }
        }
    }
}

fn get_console_mode(pipe: &PipeHandle) -> IoResult<DWORD> {
    let mut mode = 0;
    let res = unsafe { GetConsoleMode(pipe.as_handle(), &mut mode) };
    if res == 0 {
        Err(win32_error_with_context(
            "GetConsoleMode",
            IoError::last_os_error(),
        ))
    } else {
        Ok(mode)
    }
}

fn set_console_mode(pipe: &PipeHandle, mode: DWORD) -> IoResult<()> {
    let res = unsafe { SetConsoleMode(pipe.as_handle(), mode) };
    if res == 0 {
        Err(win32_error_with_context(
            "SetConsoleMode",
            IoError::last_os_error(),
        ))
    } else {
        Ok(())
    }
}

fn is_pty_stream<F: AsRawHandle>(f: &F) -> bool {
    let handle = f.as_raw_handle();
    unsafe { GetFileType(handle as _) == FILE_TYPE_CHAR }
}

impl BridgeServer {
    pub fn new() -> Self {
        let stdin_is_pty = is_pty_stream(&std::io::stdin());
        let stdout_is_pty = is_pty_stream(&std::io::stdout());
        let stderr_is_pty = is_pty_stream(&std::io::stderr());

        Self {
            stdin_is_pty,
            stdout_is_pty,
            stderr_is_pty,
            conin: None,
            conout: None,
            conin_pipe: None,
            conout_pipe: None,
            input_mode: None,
            output_mode: None,
            stderr: None,
            stdout: None,
            stdin: None,
        }
    }

    /// Creates the server pipe and returns the name of the pipe
    /// so that it can be passed to the client process
    pub fn start(&mut self, token: &Token) -> IoResult<Vec<OsString>> {
        let mut args = vec![];

        if !self.stdin_is_pty {
            let pipe = NamedPipeServer::for_token(token)?;
            self.stdin.replace(pipe.pipe);
            args.push("--stdin".into());
            args.push(pipe.path.into());
        }

        if !self.stdout_is_pty {
            let pipe = NamedPipeServer::for_token(token)?;
            self.stdout.replace(pipe.pipe);
            args.push("--stdout".into());
            args.push(pipe.path.into());
        }

        if !self.stderr_is_pty {
            let pipe = NamedPipeServer::for_token(token)?;
            self.stderr.replace(pipe.pipe);
            args.push("--stderr".into());
            args.push(pipe.path.into());
        }

        if let Ok(conin) = PipeHandle::open_pipe("CONIN$") {
            self.input_mode.replace(get_console_mode(&conin)?);
            let pipe = NamedPipeServer::for_token(token)?;
            self.conin_pipe.replace(pipe.pipe);

            args.push("--conin".into());
            args.push(pipe.path.into());

            set_console_mode(
                &conin,
                // ENABLE_PROCESSED_OUTPUT |  FIXME: CTRl-C handling?
                ENABLE_VIRTUAL_TERMINAL_INPUT,
            )?;
            self.conin.replace(conin);
        }

        if let Ok(conout) = PipeHandle::open_pipe("CONOUT$") {
            self.output_mode.replace(get_console_mode(&conout)?);
            let pipe = NamedPipeServer::for_token(token)?;
            self.conout_pipe.replace(pipe.pipe);

            args.push("--conout".into());
            args.push(pipe.path.into());

            let mut console_info: CONSOLE_SCREEN_BUFFER_INFO = unsafe { std::mem::zeroed() };
            let res = unsafe { GetConsoleScreenBufferInfo(conout.as_handle(), &mut console_info) };

            if res == 0 {
                return Err(win32_error_with_context(
                    "GetConsoleScreenBufferInfo",
                    IoError::last_os_error(),
                ));
            }

            // The console info describes the buffer dimensions.
            // We need to do a little bit of math to obtain the viewport dimensions!
            let width = console_info
                .srWindow
                .Right
                .saturating_sub(console_info.srWindow.Left) as usize
                + 1;

            args.push("--width".into());
            args.push(width.to_string().into());

            let height = console_info
                .srWindow
                .Bottom
                .saturating_sub(console_info.srWindow.Top) as usize
                + 1;

            args.push("--height".into());
            args.push(height.to_string().into());

            let cursor_x = console_info.dwCursorPosition.X as usize;
            let cursor_y = console_info
                .dwCursorPosition
                .Y
                .saturating_sub(console_info.srWindow.Top) as usize;

            args.push("--cursor-x".into());
            args.push(cursor_x.to_string().into());

            args.push("--cursor-y".into());
            args.push(cursor_y.to_string().into());

            set_console_mode(
                &conout,
                ENABLE_PROCESSED_OUTPUT
                    | ENABLE_WRAP_AT_EOL_OUTPUT
                    | ENABLE_VIRTUAL_TERMINAL_PROCESSING
                    | DISABLE_NEWLINE_AUTO_RETURN,
            )?;

            self.conout.replace(conout);
        }

        Ok(args)
    }

    pub fn serve(mut self, proc: Process) -> IoResult<DWORD> {
        if let Some(mut conin) = self.conin.take() {
            let mut conin_dest = self.conin_pipe.take().unwrap();
            conin_dest.wait_for_pipe_client()?;
            std::thread::spawn(move || std::io::copy(&mut conin, &mut conin_dest));
        }

        let conout_thread = self.conout.take().map(|mut conout| {
            let mut conout_src = self.conout_pipe.take().unwrap();
            let _ = conout_src.wait_for_pipe_client();
            std::thread::spawn(move || std::io::copy(&mut conout_src, &mut conout))
        });

        if let Some(mut stdin_dest) = self.stdin.take() {
            stdin_dest.wait_for_pipe_client()?;
            std::thread::spawn(move || {
                let mut stdin = std::io::stdin();
                let _ = std::io::copy(&mut stdin, &mut stdin_dest);
            });
        }

        let stdout_thread = self.stdout.take().map(|mut stdout_src| {
            let _ = stdout_src.wait_for_pipe_client();
            std::thread::spawn(move || {
                let mut stdout = std::io::stdout();
                let _ = std::io::copy(&mut stdout_src, &mut stdout);
            })
        });
        let stderr_thread = self.stderr.take().map(|mut stderr_src| {
            let _ = stderr_src.wait_for_pipe_client();
            std::thread::spawn(move || {
                let mut stderr = std::io::stderr();
                let _ = std::io::copy(&mut stderr_src, &mut stderr);
            })
        });

        let _ = proc.wait_for(None)?;

        stdout_thread.map(|t| t.join());
        stderr_thread.map(|t| t.join());
        conout_thread.map(|t| t.join());

        let exit_code = proc.exit_code()?;
        Ok(exit_code)
    }
}

pub fn locate_pty_bridge() -> IoResult<PathBuf> {
    let bridge_name = "eledo-pty-bridge.exe";
    let bridge_path = std::env::current_exe()?
        .parent()
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::Other,
                "current exe has no containing dir while locating pty bridge!?",
            )
        })?
        .join(bridge_name);
    if bridge_path.exists() {
        Ok(bridge_path)
    } else {
        pathsearch::find_executable_in_path(bridge_name).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::Other,
                format!(
                    "{} not found alongside executable or in the path",
                    bridge_name
                ),
            )
        })
    }
}
