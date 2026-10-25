//! The host as its own process, its standard output a pipe nobody reads:
//! a property of the binary no container produces, since the daemon drains
//! every container's output.

use std::{
    io::{Read as _, Write as _},
    net::{Ipv4Addr, TcpListener, TcpStream},
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

use anyhow::{bail, Context as _, Result};

/// Sized for a debug build's spawn on a loaded machine.
const FIRST_ANSWER: Duration = Duration::from_secs(60);
const ANSWER: Duration = Duration::from_secs(5);
/// How long the filler's count stands still before the pipe counts as full.
const FILLED: Duration = Duration::from_secs(1);

/// Killed and reaped on every exit of the test.
struct Host(Child);

impl Drop for Host {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn free_port() -> Result<u16> {
    Ok(TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?
        .local_addr()?
        .port())
}

/// The status line of `GET /live`, or the error the attempt met.
fn live(port: u16) -> Result<String> {
    let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, port))?;
    stream.set_read_timeout(Some(ANSWER))?;
    stream.write_all(b"GET /live HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")?;
    let mut answer = String::new();
    stream.read_to_string(&mut answer)?;
    Ok(answer.lines().next().unwrap_or_default().to_owned())
}

/// A pipe whose buffer is full, its reading end held and never read: the
/// first write to it waits for good. Filled from a thread that writes until a
/// write waits, which shows as its count standing still.
fn full_pipe() -> Result<(std::io::PipeReader, std::io::PipeWriter)> {
    let (reader, writer) = std::io::pipe()?;
    let mut filler = writer.try_clone()?;
    let written = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&written);
    // Ends once the reading end is dropped and its write fails.
    let _detached = std::thread::spawn(move || {
        while filler.write_all(&[b'.'; 512]).is_ok() {
            counted.fetch_add(1, Ordering::SeqCst);
        }
    });
    let mut last = usize::MAX;
    while written.load(Ordering::SeqCst) != last {
        last = written.load(Ordering::SeqCst);
        std::thread::sleep(FILLED);
    }
    Ok((reader, writer))
}

/// A host whose standard output is a full pipe nobody reads comes up and
/// answers: its log queues for a writer thread of its own, and no runtime
/// worker waits for the pipe. A host writing each line from the thread that
/// logs stops at its first line, before it listens.
#[test]
fn a_host_whose_log_nobody_reads_keeps_answering() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let port = free_port()?;
    let (_unread, stdout) = full_pipe()?;
    let mut host = Host(
        Command::new(env!("CARGO_BIN_EXE_pdn-node-http"))
            .env("PDN_DATA_DIR", dir.path())
            .env("PDN_HOST", "127.0.0.1")
            .env("PDN_PORT", port.to_string())
            .stdin(Stdio::null())
            .stdout(stdout)
            .stderr(Stdio::null())
            .spawn()
            .context("starting the host")?,
    );
    let deadline = Instant::now() + FIRST_ANSWER;
    loop {
        if let Ok(status) = live(port) {
            assert!(status.contains(" 200 "), "the host answered {status:?}");
            break;
        }
        if let Some(code) = host.0.try_wait()? {
            bail!("the host exited with {code}");
        }
        if Instant::now() > deadline {
            bail!("the host never answered with its log unread");
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    // The pipe stays full while the host keeps logging.
    for _ in 0..5 {
        std::thread::sleep(Duration::from_secs(1));
        let status = live(port).context("the host stopped answering with its log unread")?;
        assert!(status.contains(" 200 "), "the host answered {status:?}");
    }
    Ok(())
}
