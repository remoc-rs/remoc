use std::{
    io::{BufRead, BufReader},
    process::{Child, Command, ExitStatus, Stdio},
    sync::mpsc::{self, Receiver},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const LIMIT: Duration = Duration::from_secs(30);
const DAEMON: &str = env!("CARGO_BIN_EXE_timerd");
const CLIENT: &str = env!("CARGO_BIN_EXE_timerctl");

struct Process {
    child: Child,
    lines: Receiver<String>,
    reader: Option<JoinHandle<std::io::Result<()>>>,
}

impl Process {
    fn start(binary: &str, endpoint: &str, args: &[&str]) -> Self {
        let mut child = Command::new(binary)
            .args(["--endpoint", endpoint])
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("start example program");
        let stdout = child.stdout.take().unwrap();
        let (sender, lines) = mpsc::channel();
        let reader = thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                if sender.send(line?).is_err() {
                    break;
                }
            }
            Ok(())
        });
        Self { child, lines, reader: Some(reader) }
    }

    fn line(&self) -> String {
        self.lines.recv_timeout(LIMIT).expect("example output timed out or closed")
    }

    fn finish(mut self) -> (ExitStatus, String) {
        let deadline = Instant::now() + LIMIT;
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                self.reader.take().unwrap().join().unwrap().unwrap();
                let output = self.lines.try_iter().collect::<Vec<_>>().join("\n");
                return (status, output);
            }
            assert!(Instant::now() < deadline, "example command did not finish");
            thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        if self.child.try_wait().expect("check example process").is_none() {
            self.child.kill().expect("stop example process");
        }
        self.child.wait().expect("reap example process");
        if let Some(reader) = self.reader.take() {
            reader.join().expect("join output reader").expect("read example output");
        }
    }
}

fn client(endpoint: &str, args: &[&str]) -> String {
    let (status, output) = Process::start(CLIENT, endpoint, args).finish();
    assert!(status.success(), "timerctl {args:?} failed");
    output
}

#[test]
fn timers_outlive_clients_and_notify_watchers() {
    #[cfg(unix)]
    let directory = tempfile::tempdir().unwrap();
    #[cfg(unix)]
    let endpoint = directory.path().join("timerd.sock").to_str().unwrap().to_owned();
    #[cfg(windows)]
    let endpoint = format!(r"\\.\pipe\remoc-timerd-smoke-{}", std::process::id());

    let daemon = Process::start(DAEMON, &endpoint, &[]);
    assert!(daemon.line().starts_with("Listening on "));
    assert_eq!(client(&endpoint, &["list"]), "No pending timers.");

    assert_eq!(client(&endpoint, &["add", "1h", "keep running"]), "Added timer 1");
    assert_eq!(client(&endpoint, &["list"]), "1: keep running");

    let (status, _) = Process::start(DAEMON, &endpoint, &[]).finish();
    assert!(!status.success(), "a second daemon claimed the endpoint");
    assert_eq!(client(&endpoint, &["list"]), "1: keep running");

    assert_eq!(
        client(&endpoint, &["add", "0s", "instant reminder", "--wait"]),
        "Added timer 2\nFinished 2: instant reminder"
    );
    assert_eq!(client(&endpoint, &["list"]), "1: keep running");

    let first = Process::start(CLIENT, &endpoint, &["watch"]);
    assert!(first.line().starts_with("Watching timer completions."));
    let second = Process::start(CLIENT, &endpoint, &["watch"]);
    assert!(second.line().starts_with("Watching timer completions."));

    assert_eq!(client(&endpoint, &["add", "5s", "tea is ready"]), "Added timer 3");
    assert_eq!(client(&endpoint, &["list"]), "1: keep running\n3: tea is ready");
    assert_eq!(first.line(), "Finished 3: tea is ready");
    assert_eq!(second.line(), "Finished 3: tea is ready");
    assert_eq!(client(&endpoint, &["list"]), "1: keep running");

    let waiting = Process::start(CLIENT, &endpoint, &["add", "5s", "wait for tea", "--wait"]);
    assert_eq!(waiting.line(), "Added timer 4");
    assert_eq!(client(&endpoint, &["list"]), "1: keep running\n4: wait for tea");
    let (status, output) = waiting.finish();
    assert!(status.success(), "the waiting client failed");
    assert_eq!(output, "Finished 4: wait for tea");
    assert_eq!(first.line(), "Finished 4: wait for tea");
    assert_eq!(second.line(), "Finished 4: wait for tea");
    assert_eq!(client(&endpoint, &["list"]), "1: keep running");

    let abandoned = Process::start(CLIENT, &endpoint, &["add", "5s", "keep ticking", "--wait"]);
    assert_eq!(abandoned.line(), "Added timer 5");
    drop(abandoned);
    assert_eq!(client(&endpoint, &["list"]), "1: keep running\n5: keep ticking");
    assert_eq!(first.line(), "Finished 5: keep ticking");
    assert_eq!(second.line(), "Finished 5: keep ticking");
    assert_eq!(client(&endpoint, &["list"]), "1: keep running");

    drop((first, second));
    #[cfg(unix)]
    {
        assert!(Command::new("kill").args(["-INT", &daemon.child.id().to_string()]).status().unwrap().success());
        let (status, _) = daemon.finish();
        assert!(status.success(), "daemon did not shut down normally");
        assert!(!std::path::Path::new(&endpoint).exists(), "socket was not removed");
    }
    #[cfg(windows)]
    drop(daemon);
    let (status, _) = Process::start(CLIENT, &endpoint, &["list"]).finish();
    assert!(!status.success(), "the client succeeded without a daemon");
}
