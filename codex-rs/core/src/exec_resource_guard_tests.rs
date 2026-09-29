use super::*;
use codex_protocol::config_types::WindowsSandboxLevel;
use codex_protocol::models::PermissionProfile;
use pretty_assertions::assert_eq;
use std::io::BufRead;
use std::io::Write;
use std::os::unix::net::UnixListener;

#[tokio::test]
async fn guarded_tool_worker() {
    let Ok(expected) = std::env::var("GUARD_CORE_TEST_EXIT") else {
        return;
    };
    let cwd = AbsolutePathBuf::current_dir().unwrap();
    let output = process_exec_tool_call(
        ExecParams {
            command: vec!["/bin/sh".into(), "-c".into(), "printf original".into()],
            cwd: cwd.clone(),
            expiration: 5000.into(),
            capture_policy: ExecCapturePolicy::ShellTool,
            env: HashMap::new(),
            network: None,
            network_environment_id: None,
            sandbox_permissions: SandboxPermissions::UseDefault,
            windows_sandbox_level: WindowsSandboxLevel::Disabled,
            justification: None,
            arg0: None,
        },
        &PermissionProfile::Disabled,
        &cwd,
        std::slice::from_ref(&cwd),
        &None,
        &None,
        false,
        None,
    )
    .await
    .unwrap();
    let expected: i32 = expected.parse().unwrap();
    assert_eq!(output.exit_code, expected);
    assert_eq!(output.stdout.text, "original");
    assert!(output.stderr.text.contains(if expected == 137 {
        "Resource protection ended"
    } else {
        "outcome unavailable"
    }));
    assert!(output.aggregated_output.text.contains(&output.stderr.text));
}

#[test]
fn legacy_shell_preserves_resource_failures_in_tool_output() {
    for (reply, code) in [("killed memory_limit\n", "137"), ("denied\n", "125")] {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("guard.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        let mut worker = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "exec::resource_guard_tests::guarded_tool_worker",
                "--nocapture",
            ])
            .env(codex_utils_pty::resource_guard::SOCKET_ENV, &socket)
            .env("GUARD_CORE_TEST_EXIT", code)
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        for (expected, response) in [
            (
                "reserve normal\n".to_string(),
                "0123456789abcdef0123456789abcdef\n",
            ),
            (
                "attach 0123456789abcdef0123456789abcdef\n".to_string(),
                "1\n",
            ),
            (
                "result 0123456789abcdef0123456789abcdef\n".to_string(),
                reply,
            ),
        ] {
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        if Instant::now() > deadline {
                            let _ = worker.kill();
                            panic!("resource guard request timed out");
                        }
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) => panic!("{error}"),
                }
            };
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut request = String::new();
            std::io::BufReader::new(&mut stream)
                .read_line(&mut request)
                .unwrap();
            assert_eq!(request, expected);
            stream.write_all(response.as_bytes()).unwrap();
        }
        assert!(worker.wait().unwrap().success());
    }
}
