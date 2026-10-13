use pretty_assertions::assert_eq;
use std::collections::HashMap;
use std::io::Read;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixListener;
use std::process::Command;
use std::time::Duration;

use crate::resource_guard::SOCKET_ENV;

#[test]
fn guard_worker() {
    let Ok(mode) = std::env::var("GUARD_TEST_MODE") else {
        return;
    };
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
        let marker = std::env::var("GUARD_TEST_MARKER").unwrap();
        let directive = if mode == "build" { "# agent-resource-profile: build\n" } else { "" };
        let interaction = if mode == "interactive" { "read first; read second; test \"$first $second\" = 'first second' || exit 34; " } else { "" };
        let args = vec!["-c".into(), format!("{directive}test -z \"${{CODEX_RESOURCE_GUARD_SOCKET+x}}\" || exit 33; {interaction}printf executed > '{marker}'; printf result")];
        let env = HashMap::from([
            ("PATH".into(), std::env::var("PATH").unwrap()),
            (SOCKET_ENV.into(), "/untrusted/tool/environment.sock".into()),
        ]);
        let result = if mode == "pty" || mode == "interactive" {
            crate::spawn_pty_process(
                "/bin/sh", &args, std::path::Path::new("/"), &env,
                /*arg0*/ &None, crate::TerminalSize::default(), crate::ChildFds::Inherited(&[]),
            ).await
        } else {
            crate::spawn_pipe_process(
                "/bin/sh", &args, std::path::Path::new("/"), &env, /*arg0*/ &None, &[],
            ).await
        };
        if mode == "deny" {
            assert!(result.is_err());
            assert!(!std::path::Path::new(&marker).exists());
            return;
        }
        let mut process = result.unwrap();
        if mode == "interactive" {
            let writer = process.session.writer_sender();
            writer.send(b"first\n".to_vec()).await.unwrap();
            tokio::time::sleep(Duration::from_millis(50)).await;
            writer.send(b"second\n".to_vec()).await.unwrap();
        }
        let code = tokio::time::timeout(Duration::from_secs(5), &mut process.exit_rx).await.unwrap().unwrap();
        assert_eq!(code, std::env::var("GUARD_TEST_EXIT").unwrap().parse::<i32>().unwrap());
        let mut output = Vec::new();
        while let Some(chunk) = process.stdout_rx.recv().await {
            output.extend(chunk);
        }
        while let Some(chunk) = process.stderr_rx.recv().await {
            output.extend(chunk);
        }
        if code == 137 {
            assert!(String::from_utf8_lossy(&output).contains("Resource protection ended"));
        }
        if code == 125 {
            assert!(String::from_utf8_lossy(&output).contains("outcome unavailable"));
        }
    });
}

fn scenario(
    mode: &str,
    attach: u8,
    result: &str,
    expected: i32,
) -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let socket = dir.path().join("guard.sock");
    let marker = dir.path().join("executed");
    let listener = UnixListener::bind(&socket)?;
    listener.set_nonblocking(true)?;
    let token = "0123456789abcdef0123456789abcdef";
    let mut worker = Command::new(std::env::current_exe()?)
        .args([
            "--exact",
            "resource_guard_tests::guard_worker",
            "--nocapture",
        ])
        .env(SOCKET_ENV, &socket)
        .env("GUARD_TEST_MODE", mode)
        .env("GUARD_TEST_MARKER", &marker)
        .env("GUARD_TEST_EXIT", expected.to_string())
        .spawn()?;
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let mut requests = Vec::new();
    let mut peers = Vec::new();
    let count = if attach == b'1' { 3 } else { 2 };
    while requests.len() < count {
        let (mut stream, _) = match listener.accept() {
            Ok(value) => value,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if std::time::Instant::now() > deadline {
                    worker.kill()?;
                    panic!("guard protocol stalled after {requests:?}");
                }
                std::thread::sleep(Duration::from_millis(10));
                continue;
            }
            Err(error) => panic!("{error}"),
        };
        stream.set_read_timeout(Some(Duration::from_secs(2)))?;
        let mut credentials: libc::ucred = unsafe { std::mem::zeroed() };
        let mut size = std::mem::size_of_val(&credentials) as libc::socklen_t;
        assert_eq!(
            unsafe {
                libc::getsockopt(
                    stream.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_PEERCRED,
                    std::ptr::addr_of_mut!(credentials).cast(),
                    &mut size,
                )
            },
            0
        );
        peers.push(credentials.pid as u32);
        let mut line = Vec::new();
        loop {
            let mut byte = [0];
            if stream.read(&mut byte)? == 0 || byte == *b"\n" {
                break;
            }
            line.push(byte[0]);
            assert!(line.len() < 128);
        }
        requests.push(String::from_utf8(line)?);
        match requests.len() {
            1 => {
                stream.write_all(format!("{token}\n").as_bytes())?;
            }
            2 => {
                assert!(!marker.exists(), "payload ran before admission");
                stream.write_all(&[attach])?;
            }
            3 => {
                stream.write_all(result.as_bytes())?;
            }
            _ => unreachable!(),
        }
    }
    assert!(worker.wait()?.success());
    assert_eq!(
        requests[0],
        if mode == "build" {
            "reserve build"
        } else {
            "reserve normal"
        }
    );
    assert_eq!(requests[1], format!("attach {token}"));
    assert_eq!(peers[0], worker.id());
    assert_ne!(
        peers[1],
        worker.id(),
        "attachment must authenticate the child"
    );
    if attach == b'1' {
        assert_eq!(requests[2], format!("result {token}"));
        assert_eq!(peers[2], worker.id());
        assert!(marker.exists());
    }
    Ok(())
}

#[test]
fn pipe_attaches_before_payload() -> Result<(), Box<dyn std::error::Error>> {
    scenario("pipe", b'1', "ok\n", 0)
}

#[test]
fn pty_attaches_before_payload() -> Result<(), Box<dyn std::error::Error>> {
    scenario("pty", b'1', "ok\n", 0)
}

#[test]
fn interactive_writes_keep_one_resource_reservation() -> Result<(), Box<dyn std::error::Error>> {
    scenario("interactive", b'1', "ok\n", 0)
}

#[test]
fn denied_attachment_never_executes() -> Result<(), Box<dyn std::error::Error>> {
    scenario("deny", b'0', "", 0)
}

#[test]
fn recorded_kill_overrides_success() -> Result<(), Box<dyn std::error::Error>> {
    scenario("pipe", b'1', "killed memory_limit\n", 137)
}

#[test]
fn pty_kill_is_model_visible() -> Result<(), Box<dyn std::error::Error>> {
    scenario("pty", b'1', "killed memory_pressure\n", 137)
}

#[test]
fn lost_outcome_is_not_success() -> Result<(), Box<dyn std::error::Error>> {
    scenario("pipe", b'1', "", 125)
}

#[test]
fn build_budget_requires_explicit_directive() -> Result<(), Box<dyn std::error::Error>> {
    scenario("build", b'1', "ok\n", 0)
}

#[test]
fn missing_broker_does_not_run_payload() {
    let directory = tempfile::tempdir().unwrap();
    let status = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "resource_guard_tests::guard_worker",
            "--nocapture",
        ])
        .env(SOCKET_ENV, directory.path().join("missing.sock"))
        .env("GUARD_TEST_MODE", "deny")
        .env("GUARD_TEST_MARKER", directory.path().join("executed"))
        .status()
        .unwrap();
    assert!(status.success());
}
