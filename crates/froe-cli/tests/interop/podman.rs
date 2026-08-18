//! The container harness: a volume and a Sling container, each of which
//! cleans itself up on drop, and the wait that proves one is serving.

use super::*;

// ---------------------------------------------------------------------------
// Podman orchestration
// ---------------------------------------------------------------------------

/// Run a podman command; assert success and return stdout.
pub(crate) fn podman(args: &[&str]) -> String {
    let output = Command::new("podman")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap_or_else(|error| panic!("failed to spawn podman {args:?}: {error}"));
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(
        output.status.success(),
        "podman {args:?} exited with {status}\nstdout:\n{stdout}\nstderr:\n{stderr}",
        status = output.status
    );
    stdout
}

/// A podman volume that is removed on drop.
pub(crate) struct PodmanVolume {
    pub(crate) name: String,
}

impl PodmanVolume {
    pub(crate) fn new(name: &str) -> Self {
        // Remove any leftover volume from a previous run; ignore failure
        // because the volume may not exist.
        let _ = Command::new("podman").args(["volume", "rm", name]).output();
        podman(&["volume", "create", name]);
        Self {
            name: name.to_owned(),
        }
    }
}

impl Drop for PodmanVolume {
    fn drop(&mut self) {
        let _ = Command::new("podman")
            .args(["volume", "rm", &self.name])
            .output();
    }
}

/// A podman container that is stopped and removed on drop.
pub(crate) struct PodmanContainer {
    pub(crate) name: String,
}

impl PodmanContainer {
    pub(crate) fn run_detached(name: &str, port: u16, volume: &str) -> Self {
        let port_arg = format!("{port}:8080");
        let volume_arg = format!("{volume}:/opt/sling/launcher");
        podman(&[
            "run",
            "-d",
            "--name",
            name,
            "-p",
            &port_arg,
            "-v",
            &volume_arg,
            &sling_image(),
        ]);
        Self {
            name: name.to_owned(),
        }
    }

    pub(crate) fn stop(&self) {
        let _ = Command::new("podman").args(["stop", &self.name]).output();
        let _ = Command::new("podman").args(["rm", &self.name]).output();
    }

    /// Kills the JVM outright, the way an OOM kill or a yanked host does.
    ///
    /// [`Self::stop`] is graceful — SIGTERM with a grace period — and Oak's
    /// shutdown hook comfortably beats it, which is exactly the behaviour
    /// this must not have: a cleanly closed archive carries its index, and
    /// the condition under test never arises. The image's entrypoint `exec`s
    /// the JVM, so PID 1 in the container *is* Oak and the signal lands on
    /// it with no shell in between.
    pub(crate) fn kill_uncleanly(&self) {
        podman(&["kill", "-s", "KILL", &self.name]);
        // Reaping is not synchronous with the kill returning. The exit code
        // is the evidence that the JVM died on the signal rather than
        // exiting, so it is read before `Drop` removes the container.
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let status = podman(&[
                "inspect",
                "-f",
                "{{.State.Status}} {{.State.ExitCode}}",
                &self.name,
            ]);
            let status = status.trim();
            if let Some(code) = status.strip_prefix("exited ") {
                assert_eq!(
                    code, "137",
                    "the JVM must have died on SIGKILL (128 + 9), not exited on its own; \
                     a clean exit means Oak closed its archives and wrote their indexes, \
                     so the condition this phase exercises would not exist"
                );
                return;
            }
            assert!(
                Instant::now() < deadline,
                "the container did not report an exit status after SIGKILL: {status}"
            );
            std::thread::sleep(Duration::from_millis(200));
        }
    }
}

impl Drop for PodmanContainer {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Wait for Sling to finish booting and report all bundles active.
pub(crate) fn wait_for_sling(port: u16, container_name: &str) {
    let deadline = Instant::now() + SLING_BOOT_TIMEOUT;
    loop {
        if Instant::now() > deadline {
            let logs = Command::new("podman")
                .args(["logs", container_name])
                .output()
                .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
                .unwrap_or_default();
            panic!(
                "Sling did not come up at :{port} within {SLING_BOOT_TIMEOUT:?}\nlast logs:\n{logs}"
            );
        }

        let output = Command::new("curl")
            .args([
                "-s",
                "-o",
                "/dev/null",
                "-w",
                "%{http_code}",
                "-u",
                "admin:admin",
                &format!("http://localhost:{port}/system/console/bundles.json"),
            ])
            .output();

        if let Ok(out) = output {
            let code = String::from_utf8_lossy(&out.stdout);
            if code.trim() == "200" {
                // Confirm all bundles are active (one fragment may stay resolved).
                let json = Command::new("curl")
                    .args([
                        "-s",
                        "-u",
                        "admin:admin",
                        &format!("http://localhost:{port}/system/console/bundles.json"),
                    ])
                    .output()
                    .expect("curl bundles.json");
                let body = String::from_utf8_lossy(&json.stdout);
                // The "s" field is [total, active, active.fragments, ...].
                // Sling ships one fragment; ready when resolved count is 0.
                if let Some(resolved) = extract_bundle_count(&body, 3)
                    && resolved == 0
                {
                    return;
                }
            }
        }
        std::thread::sleep(Duration::from_secs(5));
    }
}

/// Parse the Felix web console JSON to extract a count from the "s" array.
pub(crate) fn extract_bundle_count(json: &str, index: usize) -> Option<i64> {
    // The JSON has "s":[total, active, fragments, resolved, ...].
    // Rather than pull in a JSON dependency, find the array by key.
    let key = "\"s\":[";
    let pos = json.find(key)?;
    let rest = &json[pos + key.len()..];
    let end = rest.find(']')?;
    let numbers: Vec<i64> = rest[..end]
        .split(',')
        .map(|s| s.trim().parse().ok())
        .collect::<Option<Vec<i64>>>()?;
    numbers.get(index).copied()
}
