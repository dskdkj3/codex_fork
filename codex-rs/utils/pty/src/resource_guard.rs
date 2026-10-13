//! Optional host-owned Linux resource admission for tool processes.
//!
//! The controller reserves a job before fork. The child authenticates to the
//! host broker with SO_PEERCRED and waits for cgroup attachment before exec.
//! This is independent of sandbox/approval policy. Missing or failed brokers
//! never silently turn a configured guard off.

use std::io;
use std::io::Read;
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

pub const SOCKET_ENV: &str = "CODEX_RESOURCE_GUARD_SOCKET";
pub const BUILD_DIRECTIVE: &str = "# agent-resource-profile: build";
const MAX_REPLY: u64 = 256;

pub struct ResourceGuard {
    socket: PathBuf,
    token: String,
    attachment: ChildAttachment,
}

/// Preallocated attachment request; safe to use after fork, before exec.
#[derive(Clone, Copy)]
pub struct ChildAttachment {
    address: libc::sockaddr_un,
    request: [u8; 40],
}

pub struct GuardOutcome {
    pub exit_code: i32,
    pub diagnostic: Option<String>,
}

impl ResourceGuard {
    pub async fn prepare(args: &[String]) -> io::Result<Option<Self>> {
        let Some(socket) = std::env::var_os(SOCKET_ENV) else {
            return Ok(None);
        };
        let socket = PathBuf::from(socket);
        // An explicit first-line directive selects a previously approved
        // budget, never a new arbitrary limit. It is still a shell comment.
        let profile = if args
            .iter()
            .any(|arg| arg.lines().next() == Some(BUILD_DIRECTIVE))
        {
            "build"
        } else {
            "normal"
        };
        tokio::task::spawn_blocking(move || Self::reserve(socket, profile))
            .await
            .map_err(io::Error::other)?
            .map(Some)
    }

    fn reserve(socket: PathBuf, profile: &str) -> io::Result<Self> {
        let path = socket.as_os_str().as_bytes();
        let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
        if !socket.is_absolute() || path.contains(&0) || path.len() >= address.sun_path.len() {
            return Err(io::Error::other("invalid resource guard socket path"));
        }
        address.sun_family = libc::AF_UNIX as _;
        for (dest, source) in address.sun_path.iter_mut().zip(path) {
            *dest = *source as _;
        }
        let token = request(&socket, &format!("reserve {profile}\n"))?;
        if token.len() != 32 || !token.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(io::Error::other(format!(
                "resource admission refused: {token}"
            )));
        }
        let mut attachment_request = [0u8; 40];
        attachment_request[..7].copy_from_slice(b"attach ");
        attachment_request[7..39].copy_from_slice(token.as_bytes());
        attachment_request[39] = b'\n';
        Ok(Self {
            socket,
            token,
            attachment: ChildAttachment {
                address,
                request: attachment_request,
            },
        })
    }

    pub fn attachment(&self) -> ChildAttachment {
        self.attachment
    }

    pub async fn finish(self, exit_code: i32) -> GuardOutcome {
        tokio::task::spawn_blocking(move || self.finish_blocking(exit_code))
            .await
            .unwrap_or_else(|_| unavailable())
    }

    pub fn finish_blocking(self, exit_code: i32) -> GuardOutcome {
        let result = request(&self.socket, &format!("result {}\n", self.token));
        match result {
            Ok(reply) if reply == "ok" => GuardOutcome {
                exit_code,
                diagnostic: None,
            },
            Ok(reply) if reply.starts_with("killed ") => GuardOutcome {
                exit_code: 137,
                diagnostic: Some(format!(
                    "Resource protection ended this tool job ({reply}). The conversation is still running. Narrow the operation or request an appropriate budget; do not retry the same operation unchanged.\n"
                )),
            },
            _ => unavailable(),
        }
    }
}

fn unavailable() -> GuardOutcome {
    GuardOutcome {
        exit_code: 125,
        diagnostic: Some("Resource guard outcome unavailable. The command must not be reported as successful; inspect the host protection record before retrying.\n".into()),
    }
}

fn request(socket: &std::path::Path, message: &str) -> io::Result<String> {
    let mut stream = UnixStream::connect(socket)?;
    stream.set_read_timeout(Some(Duration::from_secs(35)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    stream.write_all(message.as_bytes())?;
    stream.shutdown(std::net::Shutdown::Write)?;
    let mut reply = String::new();
    stream.take(MAX_REPLY).read_to_string(&mut reply)?;
    Ok(reply.trim().to_owned())
}

impl ChildAttachment {
    /// Uses only preallocated data and async-signal-safe libc operations.
    /// The broker derives the child PID from socket credentials, not the wire.
    pub fn attach(&self) -> io::Result<()> {
        unsafe {
            let fd = libc::socket(libc::AF_UNIX, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0);
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            let result = self.attach_fd(fd);
            libc::close(fd);
            result
        }
    }

    unsafe fn attach_fd(&self, fd: libc::c_int) -> io::Result<()> {
        let timeout = libc::timeval {
            tv_sec: 10,
            tv_usec: 0,
        };
        for option in [libc::SO_RCVTIMEO, libc::SO_SNDTIMEO] {
            if unsafe {
                libc::setsockopt(
                    fd,
                    libc::SOL_SOCKET,
                    option,
                    std::ptr::addr_of!(timeout).cast(),
                    std::mem::size_of_val(&timeout) as _,
                )
            } < 0
            {
                return Err(io::Error::last_os_error());
            }
        }
        if unsafe {
            libc::connect(
                fd,
                std::ptr::addr_of!(self.address).cast(),
                std::mem::size_of_val(&self.address) as _,
            )
        } < 0
        {
            return Err(io::Error::last_os_error());
        }
        let mut sent = 0;
        while sent < self.request.len() {
            let n = unsafe {
                libc::send(
                    fd,
                    self.request[sent..].as_ptr().cast(),
                    self.request.len() - sent,
                    libc::MSG_NOSIGNAL,
                )
            };
            if n <= 0 {
                return Err(io::Error::last_os_error());
            }
            sent += n as usize;
        }
        let mut reply = [0u8; 1];
        if unsafe { libc::recv(fd, reply.as_mut_ptr().cast(), 1, 0) } != 1 || reply != *b"1" {
            return Err(io::Error::from_raw_os_error(libc::EACCES));
        }
        Ok(())
    }
}
