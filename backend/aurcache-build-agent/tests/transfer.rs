//! The transfer subcommands, as the binary a worker execs: run on temporary
//! directories, judged by their stdout, stderr and exit status.

use aurcache_build_agent::layout::Work;
use aurcache_build_agent::report::{Artifact, BuildReport, Ended, Handback};
use aurcache_build_agent::spec::INTERFACE;
use sha2::{Digest, Sha256};
use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::os::unix::fs::symlink;
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};
use tar::{Builder, EntryType, Header};

/// How long a subcommand may take to do what a test waits for.
const PATIENCE: Duration = Duration::from_secs(20);

fn agent<I, S>(args: I) -> Command
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut command = Command::new(env!("CARGO_BIN_EXE_aurcache-build-agent"));
    command.args(args);
    command
}

/// What a finished subcommand left.
struct Ran {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: String,
}

fn run(command: &mut Command) -> Ran {
    let output = command.output().unwrap();
    Ran {
        status: output.status,
        stdout: output.stdout,
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

/// Wait for `child` to end, killing it and failing if it takes too long.
fn finish(child: &mut Child) -> ExitStatus {
    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        if start.elapsed() > PATIENCE {
            child.kill().unwrap();
            panic!("the subcommand did not end");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Whether `child` is still running after a moment.
fn lingers(child: &mut Child) -> bool {
    std::thread::sleep(Duration::from_millis(500));
    child.try_wait().unwrap().is_none()
}

fn until(what: &str, mut done: impl FnMut() -> bool) {
    let start = Instant::now();
    while !done() {
        assert!(start.elapsed() < PATIENCE, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn package(name: &str) -> String {
    format!("{name}-1-1-x86_64.pkg.tar.zst")
}

fn sha256(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

fn write_report(work: &Work) {
    let report = BuildReport {
        interface: INTERFACE,
        ended: Ended::Exited { code: 0 },
        peak_memory_bytes: Some(1),
        oom_kills: Some(0),
        disk_bytes: None,
        vcs_commits: Default::default(),
    };
    fs::create_dir_all(work.out()).unwrap();
    fs::write(work.report(), serde_json::to_vec(&report).unwrap()).unwrap();
}

// ---- receive and stage ------------------------------------------------------

/// One tar entry of any type and name, including names `set_path` refuses.
fn entry(builder: &mut Builder<Vec<u8>>, kind: EntryType, name: &str, data: &[u8]) {
    let mut header = Header::new_gnu();
    header.set_entry_type(kind);
    header.set_size(data.len() as u64);
    header.set_mode(0o644);
    header.as_old_mut().name[..name.len()].copy_from_slice(name.as_bytes());
    if kind == EntryType::Symlink {
        header.set_link_name("/etc/passwd").unwrap();
    }
    header.set_cksum();
    builder.append(&header, data).unwrap();
}

/// A tar with an end-of-archive marker.
fn job_tar(entries: &[(EntryType, &str, &[u8])]) -> Vec<u8> {
    let mut builder = Builder::new(Vec::new());
    for (kind, name, data) in entries {
        entry(&mut builder, *kind, name, data);
    }
    builder.into_inner().unwrap()
}

fn good_tar() -> Vec<u8> {
    job_tar(&[
        (EntryType::Regular, "spec.json", br#"{"a":1}"#),
        (EntryType::Regular, "source.tar.gz", b"source bytes"),
    ])
}

fn receive_into(work: &Path, input: &[u8]) -> Ran {
    let mut child = agent(["receive", "--work"])
        .arg(work)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // The child may refuse and exit before the whole tar is written.
    let _ = child.stdin.take().unwrap().write_all(input);
    let output = child.wait_with_output().unwrap();
    Ran {
        status: output.status,
        stdout: output.stdout,
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

fn names_in(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .map(|entries| {
            entries
                .map(|e| e.unwrap().file_name().into_string().unwrap())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

#[test]
fn receive_round_trips_both_inputs_and_marks_ready() {
    let dir = tempfile::tempdir().unwrap();
    let work = Work::new(dir.path());
    let ran = receive_into(dir.path(), &good_tar());
    assert!(ran.status.success(), "{}", ran.stderr);
    assert!(ran.stdout.is_empty());
    assert_eq!(fs::read(work.spec()).unwrap(), br#"{"a":1}"#);
    assert_eq!(fs::read(work.source()).unwrap(), b"source bytes");
    assert!(work.ready().exists());
    assert_eq!(
        names_in(&work.input()),
        ["ready", "source.tar.gz", "spec.json"]
    );
}

#[test]
fn receive_takes_the_entries_in_either_order() {
    let dir = tempfile::tempdir().unwrap();
    let input = job_tar(&[
        (EntryType::Regular, "source.tar.gz", b"s"),
        (EntryType::Regular, "spec.json", b"{}"),
    ]);
    assert!(receive_into(dir.path(), &input).status.success());
    assert!(Work::new(dir.path()).ready().exists());
}

#[test]
fn receive_stops_at_the_end_marker_with_stdin_still_open() {
    let dir = tempfile::tempdir().unwrap();
    let mut child = agent(["receive", "--work"])
        .arg(dir.path())
        .stdin(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Held, not dropped: the sender never closes its end.
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(&good_tar()).unwrap();
    stdin.flush().unwrap();
    assert!(finish(&mut child).success());
    assert!(Work::new(dir.path()).ready().exists());
    drop(stdin);
}

#[test]
fn receive_refuses_what_is_not_the_two_files() {
    use EntryType::{Regular, Symlink};
    let refused: [&[(EntryType, &str, &[u8])]; 6] = [
        &[
            (Regular, "spec.json", b"{}"),
            (Regular, "source.tar.gz", b"s"),
            (Regular, "third", b""),
        ],
        &[
            (Regular, "spec.json", b"{}"),
            (Symlink, "source.tar.gz", b""),
        ],
        &[(Regular, "spec.json", b"{}"), (Regular, "../x", b"x")],
        &[
            (Regular, "spec.json", b"{}"),
            (Regular, "/etc/passwd", b"x"),
        ],
        &[(Regular, "spec.json", b"{}"), (Regular, "spec.json", b"{}")],
        &[(Regular, "spec.json", b"{}")],
    ];
    for entries in refused {
        let dir = tempfile::tempdir().unwrap();
        let ran = receive_into(dir.path(), &job_tar(entries));
        assert!(!ran.status.success(), "{entries:?}");
        assert!(
            ran.stderr.contains("aurcache-build-agent"),
            "{}",
            ran.stderr
        );
        let work = Work::new(dir.path());
        assert!(!work.ready().exists(), "{entries:?}");
        assert_eq!(names_in(&work.input()), Vec::<String>::new(), "{entries:?}");
        assert!(!dir.path().join("x").exists());
    }
}

#[test]
fn receive_does_not_write_through_a_link_left_in_the_directory() {
    let dir = tempfile::tempdir().unwrap();
    let work = Work::new(dir.path());
    fs::create_dir_all(work.input()).unwrap();
    let outside = dir.path().join("outside");
    fs::write(&outside, b"untouched").unwrap();
    symlink(&outside, work.input().join(".spec.json.part")).unwrap();
    let ran = receive_into(dir.path(), &good_tar());
    assert!(ran.status.success(), "{}", ran.stderr);
    assert_eq!(fs::read(&outside).unwrap(), b"untouched");
}

#[test]
fn stage_installs_the_binary_and_ends_only_once_ready_exists() {
    let dir = tempfile::tempdir().unwrap();
    let work = Work::new(dir.path().join("work"));
    let tools = dir.path().join("tools");
    let mut child = agent(["stage", "--work"])
        .arg(work.root())
        .arg("--tools")
        .arg(&tools)
        .spawn()
        .unwrap();
    until("the binary to be installed", || {
        tools.join("aurcache-build-agent").exists() && work.input().is_dir()
    });
    assert!(lingers(&mut child), "stage ended before the job was ready");
    File::create(work.ready()).unwrap();
    assert!(finish(&mut child).success());

    let installed = tools.join("aurcache-build-agent");
    let mode =
        std::os::unix::fs::PermissionsExt::mode(&fs::metadata(&installed).unwrap().permissions());
    assert_eq!(mode & 0o777, 0o755);
    let ran = run(Command::new(&installed).arg("--interface"));
    assert_eq!(ran.stdout, format!("{INTERFACE}\n").as_bytes());
}

/// The ready mark names the `receive` that made it, and `stage`, whose
/// container ending would take that process down, waits for it to exit.
#[test]
fn stage_waits_for_the_process_that_marked_the_job_ready() {
    let dir = tempfile::tempdir().unwrap();
    let work = Work::new(dir.path().join("work"));
    fs::create_dir_all(work.input()).unwrap();
    let mut receiver = Command::new("sleep").arg("60").spawn().unwrap();
    fs::write(work.ready(), format!("{}\n", receiver.id())).unwrap();
    let mut child = agent(["stage", "--work"])
        .arg(work.root())
        .arg("--tools")
        .arg(dir.path().join("tools"))
        .spawn()
        .unwrap();
    assert!(lingers(&mut child), "stage ended while receive still ran");
    receiver.kill().unwrap();
    receiver.wait().unwrap();
    assert!(finish(&mut child).success());
}

#[test]
fn receive_marks_the_job_ready_with_its_own_process() {
    let dir = tempfile::tempdir().unwrap();
    let ran = receive_into(dir.path(), &good_tar());
    assert!(ran.status.success(), "{}", ran.stderr);
    let mark = fs::read_to_string(Work::new(dir.path()).ready()).unwrap();
    assert!(mark.trim().parse::<u32>().is_ok(), "{mark:?}");
}

#[test]
fn stage_ends_at_once_when_the_job_is_already_ready() {
    let dir = tempfile::tempdir().unwrap();
    let work = Work::new(dir.path().join("work"));
    fs::create_dir_all(work.input()).unwrap();
    File::create(work.ready()).unwrap();
    let ran = run(agent(["stage", "--work"])
        .arg(work.root())
        .arg("--tools")
        .arg(dir.path().join("tools")));
    assert!(ran.status.success(), "{}", ran.stderr);
}

// ---- hold -------------------------------------------------------------------

#[test]
fn hold_waits_and_exits_zero_on_sigterm() {
    let mut child = agent(["hold"]).spawn().unwrap();
    assert!(lingers(&mut child));
    // SAFETY: signalling a child this test owns.
    unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
    assert_eq!(finish(&mut child).code(), Some(0));
}

// ---- tail -------------------------------------------------------------------

/// Read from `child`'s stdout until `want` has arrived in full.
fn read_exactly(child: &mut Child, want: &[u8]) {
    let out = child.stdout.as_mut().unwrap();
    let mut got = vec![0; want.len()];
    out.read_exact(&mut got).unwrap();
    assert_eq!(got, want);
}

fn append(path: &Path, data: &[u8]) {
    fs::OpenOptions::new()
        .append(true)
        .open(path)
        .unwrap()
        .write_all(data)
        .unwrap();
}

#[test]
fn tail_resumes_mid_file_follows_appends_and_ends_with_the_report() {
    let dir = tempfile::tempdir().unwrap();
    let work = Work::new(dir.path());
    let mut child = agent(["tail", "--from", "4", "--work"])
        .arg(dir.path())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    // The log does not exist yet: it waits for it.
    assert!(lingers(&mut child));
    fs::create_dir_all(work.log().parent().unwrap()).unwrap();
    fs::write(work.log(), b"0123456789\n").unwrap();
    read_exactly(&mut child, b"456789\n");
    append(&work.log(), b"more\n");
    read_exactly(&mut child, b"more\n");
    assert!(lingers(&mut child), "tail ended without a report");
    append(&work.log(), b"last\n");
    write_report(&work);
    let mut rest = Vec::new();
    child.stdout.take().unwrap().read_to_end(&mut rest).unwrap();
    assert_eq!(rest, b"last\n");
    assert_eq!(finish(&mut child).code(), Some(0));
}

#[test]
fn tail_ends_when_the_report_exists_and_no_log_ever_did() {
    let dir = tempfile::tempdir().unwrap();
    write_report(&Work::new(dir.path()));
    let ran = run(agent(["tail", "--from", "0", "--work"]).arg(dir.path()));
    assert!(ran.status.success(), "{}", ran.stderr);
    assert!(ran.stdout.is_empty());
}

#[test]
fn tail_stops_when_its_reader_goes_away() {
    let dir = tempfile::tempdir().unwrap();
    let work = Work::new(dir.path());
    fs::create_dir_all(work.log().parent().unwrap()).unwrap();
    fs::write(work.log(), b"line\n").unwrap();
    let mut child = agent(["tail", "--from", "0", "--work"])
        .arg(dir.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    read_exactly(&mut child, b"line\n");
    // Nothing more is ever written: only the closed pipe can end it.
    drop(child.stdout.take());
    assert!(!finish(&mut child).success());
}

// ---- manifest ---------------------------------------------------------------

fn manifest_of(work: &Path) -> Handback {
    let ran = run(agent(["manifest", "--work"]).arg(work));
    assert!(ran.status.success(), "{}", ran.stderr);
    assert_eq!(ran.stdout.last(), Some(&b'\n'));
    serde_json::from_slice(&ran.stdout).unwrap()
}

#[test]
fn manifest_lists_only_regular_files_with_package_names() {
    let dir = tempfile::tempdir().unwrap();
    let work = Work::new(dir.path());
    fs::create_dir_all(work.pkg()).unwrap();
    let pkg = work.pkg();
    fs::write(pkg.join(package("zed")), b"zed bytes").unwrap();
    fs::write(pkg.join(package("alpha")), b"").unwrap();
    fs::write(pkg.join(format!("{}.sig", package("alpha"))), b"sig").unwrap();
    fs::write(pkg.join("PKGBUILD"), b"x").unwrap();
    fs::create_dir(pkg.join(package("dir"))).unwrap();
    symlink("/etc/passwd", pkg.join(package("link"))).unwrap();
    symlink(pkg.join(package("zed")), pkg.join(package("alias"))).unwrap();
    let fifo = std::ffi::CString::new(pkg.join(package("fifo")).to_str().unwrap()).unwrap();
    // SAFETY: a valid, NUL-terminated path.
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o644) }, 0);
    write_report(&work);

    let handback = manifest_of(dir.path());
    assert_eq!(handback.interface, INTERFACE);
    assert_eq!(
        handback.artifacts,
        [
            Artifact {
                name: package("alpha"),
                size: 0,
                sha256: sha256(b""),
            },
            Artifact {
                name: package("zed"),
                size: 9,
                sha256: sha256(b"zed bytes"),
            },
        ]
    );
    let report = handback.report.unwrap();
    assert_eq!(report.ended, Ended::Exited { code: 0 });
    assert_eq!(report.peak_memory_bytes, Some(1));
}

#[test]
fn manifest_without_a_report_or_packages_says_so() {
    let dir = tempfile::tempdir().unwrap();
    let handback = manifest_of(dir.path());
    assert_eq!(handback.report, None);
    assert!(handback.artifacts.is_empty());
}

#[test]
fn manifest_refuses_a_report_it_cannot_read() {
    let dir = tempfile::tempdir().unwrap();
    let work = Work::new(dir.path());
    fs::create_dir_all(work.out()).unwrap();
    fs::write(work.report(), b"not json").unwrap();
    let ran = run(agent(["manifest", "--work"]).arg(dir.path()));
    assert!(!ran.status.success());
    assert!(ran.stdout.is_empty());
    assert!(ran.stderr.contains("report"), "{}", ran.stderr);
}

// ---- send -------------------------------------------------------------------

fn send_of(work: &Path, name: &str) -> Ran {
    run(agent(["send", "--work"]).arg(work).args(["--name", name]))
}

#[test]
fn send_writes_the_bytes_and_nothing_else() {
    let dir = tempfile::tempdir().unwrap();
    let work = Work::new(dir.path());
    fs::create_dir_all(work.pkg()).unwrap();
    let data: Vec<u8> = (0..300_000).map(|i| (i % 253) as u8).collect();
    fs::write(work.pkg().join(package("a")), &data).unwrap();
    let ran = send_of(dir.path(), &package("a"));
    assert!(ran.status.success(), "{}", ran.stderr);
    assert_eq!(ran.stdout, data);
    assert!(ran.stderr.is_empty());
}

#[test]
fn send_refuses_what_is_not_a_regular_package_file() {
    let dir = tempfile::tempdir().unwrap();
    let work = Work::new(dir.path());
    fs::create_dir_all(work.pkg()).unwrap();
    let secret = dir.path().join("secret");
    fs::write(&secret, b"secret").unwrap();
    symlink(&secret, work.pkg().join(package("link"))).unwrap();
    fs::create_dir(work.pkg().join(package("dir"))).unwrap();
    fs::write(work.pkg().join("plain.txt"), b"x").unwrap();
    fs::write(work.pkg().join(package("a")), b"x").unwrap();
    fs::write(dir.path().join(package("up")), b"escaped").unwrap();
    let fifo = std::ffi::CString::new(work.pkg().join(package("fifo")).to_str().unwrap()).unwrap();
    // SAFETY: a valid, NUL-terminated path.
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o644) }, 0);

    let up = format!("../{}", package("up"));
    let nested = format!("sub/{}", package("a"));
    let absolute = secret.display().to_string();
    let names = [
        package("link"),
        package("dir"),
        package("fifo"),
        package("missing"),
        "plain.txt".to_owned(),
        format!("{}.sig", package("a")),
        up,
        nested,
        absolute,
        String::new(),
    ];
    for name in names {
        let ran = send_of(dir.path(), &name);
        assert_eq!(ran.status.code(), Some(1), "{name:?}");
        assert!(ran.stdout.is_empty(), "{name:?}");
        assert!(ran.stderr.starts_with("aurcache-build-agent: "), "{name:?}");
    }
}

#[test]
fn send_ends_with_an_error_when_its_reader_goes_away() {
    let dir = tempfile::tempdir().unwrap();
    let work = Work::new(dir.path());
    fs::create_dir_all(work.pkg()).unwrap();
    let file = File::create(work.pkg().join(package("big"))).unwrap();
    file.set_len(1 << 30).unwrap();
    let mut child = agent(["send", "--work"])
        .arg(dir.path())
        .args(["--name", &package("big")])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut first = [0; 16];
    child
        .stdout
        .as_mut()
        .unwrap()
        .read_exact(&mut first)
        .unwrap();
    drop(child.stdout.take());
    assert!(!finish(&mut child).success());
}

// ---- bounded memory ---------------------------------------------------------

/// What a subcommand's run measured of itself.
struct Measured {
    exit_code: i32,
    /// The largest resident set it had, in bytes.
    peak_rss: u64,
}

/// Run `command`, feed each piece of its stdout to `sink`, and measure the
/// child alone (not the test process or its siblings).
// `wait4` reaps the child, which the lint cannot see.
#[allow(clippy::zombie_processes)]
fn measure(command: &mut Command, mut sink: impl FnMut(&[u8])) -> Measured {
    let mut child = command.stdout(Stdio::piped()).spawn().unwrap();
    let mut out = child.stdout.take().unwrap();
    let mut buffer = vec![0; 1 << 20];
    loop {
        let read = out.read(&mut buffer).unwrap();
        if read == 0 {
            break;
        }
        sink(&buffer[..read]);
    }
    let mut status = 0;
    // SAFETY: an all-zero `rusage` is a valid value to be overwritten.
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    // SAFETY: `status` and `usage` are live, and the child is this test's own.
    let waited = unsafe { libc::wait4(child.id() as i32, &raw mut status, 0, &raw mut usage) };
    assert_eq!(waited as u32, child.id());
    Measured {
        exit_code: libc::WEXITSTATUS(status),
        peak_rss: usage.ru_maxrss as u64 * 1024,
    }
}

/// The most resident memory either subcommand may reach: a few buffers and
/// the runtime, never a file's worth.
const RSS_LIMIT: u64 = 16 << 20;

/// A sparse package of `size` zero bytes, in a fresh work directory.
fn sparse_package(size: u64) -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().unwrap();
    let work = Work::new(dir.path());
    fs::create_dir_all(work.pkg()).unwrap();
    let name = package("huge");
    File::create(work.pkg().join(&name))
        .unwrap()
        .set_len(size)
        .unwrap();
    (dir, name)
}

/// `manifest` hashes a file in bounded memory: here 1 GiB of zeros.
#[test]
fn manifest_hashes_a_large_package_in_bounded_memory() {
    const SIZE: u64 = 1 << 30;
    // `head -c 1073741824 /dev/zero | sha256sum`
    const DIGEST: &str = "49bc20df15e412a64472421e13fe86ff1c5165e18b2afccf160d4dc19fe68a14";
    let (dir, name) = sparse_package(SIZE);
    let mut manifest = Vec::new();
    let measured = measure(agent(["manifest", "--work"]).arg(dir.path()), |piece| {
        manifest.extend_from_slice(piece);
    });
    assert_eq!(measured.exit_code, 0);
    assert!(
        measured.peak_rss < RSS_LIMIT,
        "manifest reached {} bytes resident",
        measured.peak_rss
    );
    let handback: Handback = serde_json::from_slice(&manifest).unwrap();
    assert_eq!(
        handback.artifacts,
        [Artifact {
            name,
            size: SIZE,
            sha256: DIGEST.to_owned()
        }]
    );
}

/// `send` streams a file past 32-bit sizes, here 3 GiB, without holding it.
#[test]
fn send_streams_a_multi_gigabyte_package_in_bounded_memory() {
    const SIZE: u64 = 3 << 30;
    let zeros = vec![0_u8; 1 << 20];
    let (dir, name) = sparse_package(SIZE);
    let mut count = 0_u64;
    let mut all_zero = true;
    let measured = measure(
        agent(["send", "--work"])
            .arg(dir.path())
            .args(["--name", &name]),
        |piece| {
            count += piece.len() as u64;
            all_zero &= piece == &zeros[..piece.len()];
        },
    );
    assert_eq!(measured.exit_code, 0);
    assert!(
        measured.peak_rss < RSS_LIMIT,
        "send reached {} bytes resident",
        measured.peak_rss
    );
    assert_eq!(count, SIZE);
    assert!(all_zero);
}
