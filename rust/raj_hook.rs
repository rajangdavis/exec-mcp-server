//! Runs `raj hook ...` for the exec server: the hooks are the commands, so an
//! agent names a hook and the values its author declared, never argv.
//!
//! The raj client is always pointed at a TCP control address with the token.
//! Over TCP the editor treats the caller as an agent, so only hooks the author
//! marked `--agent` run; the Unix socket would make every call the human's.
//! The child's environment is cleared to PATH, HOME and those two variables, so
//! no XDG_RUNTIME_DIR is left for the client to discover the socket through.

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Output kept from one run: the last bytes, since a failing build ends with
/// the part worth reading.
const MAX_OUT: usize = 200_000;

/// Run `raj hook <args>`, where `args` holds one argument per line. Returns the
/// combined output; a nonzero exit, a timeout or a refusal comes back as text
/// starting with "error:".
pub fn raj_hook(
    addr: impl AsRef<str>,
    token: impl AsRef<str>,
    args: impl AsRef<str>,
    timeout_secs: i64,
) -> String {
    let addr = addr.as_ref();
    if !addr.starts_with("tcp://") {
        return format!("error: RAJ_EXEC_ADDR must be a tcp:// address, so hooks run as an agent; got {addr:?}");
    }
    let argv: Vec<&str> = args.as_ref().split('\n').filter(|a| !a.is_empty()).collect();
    let timeout = Duration::from_secs(timeout_secs.clamp(1, 86_400) as u64);

    let mut cmd = Command::new("raj");
    cmd.arg("hook")
        .args(&argv)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("RAJ_CONTROL_ADDR", addr)
        .env("RAJ_CONTROL_TOKEN", token.as_ref())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(home) = std::env::var_os("HOME") {
        cmd.env("HOME", home);
    }
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => return format!("error: cannot start raj: {e}"),
    };
    let readers: Vec<_> = [child.stdout.take().map(|r| Box::new(r) as Box<dyn Read + Send>),
                           child.stderr.take().map(|r| Box::new(r) as Box<dyn Read + Send>)]
        .into_iter()
        .map(|r| {
            std::thread::spawn(move || {
                let mut buf = Vec::new();
                if let Some(mut r) = r {
                    let _ = r.read_to_end(&mut buf);
                }
                buf
            })
        })
        .collect();

    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if start.elapsed() > timeout => {
                let _ = child.kill();
                let _ = child.wait();
                return format!(
                    "error: raj hook {} did not answer within {} s; the run may still be going: check list_runs",
                    argv.first().copied().unwrap_or(""),
                    timeout.as_secs()
                );
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(e) => return format!("error: waiting for raj: {e}"),
        }
    };
    let mut out = Vec::new();
    for r in readers {
        out.extend(r.join().unwrap_or_default());
    }
    let mut text = String::from_utf8_lossy(&out).into_owned();
    if text.len() > MAX_OUT {
        let mut cut = text.len() - MAX_OUT;
        while !text.is_char_boundary(cut) {
            cut += 1;
        }
        text = format!("[... {cut} bytes cut ...]\n{}", &text[cut..]);
    }
    let text = text.trim_end().to_string();
    if status.success() {
        text
    } else {
        format!("error: raj hook exited with {status}\n{text}")
    }
}

// Jobs: a run that outlasts the caller's wait keeps going here, and its output
// is kept until a caller collects it. A hook that is not detached keeps no log
// of its own, and an MCP client gives up long before a test suite ends.

use std::collections::HashMap;
use std::sync::{Arc, Condvar, Mutex, OnceLock};

/// Jobs kept for collection; past this the oldest finished one is dropped.
const MAX_JOBS: usize = 20;

struct Job {
    hook: String,
    started: Instant,
    done: Mutex<Option<String>>,
    cv: Condvar,
}

type Jobs = Mutex<(u64, HashMap<u64, Arc<Job>>)>;

static JOBS: OnceLock<Jobs> = OnceLock::new();

fn jobs() -> &'static Jobs {
    JOBS.get_or_init(|| Mutex::new((0, HashMap::new())))
}

/// Start `raj hook <args>` (one argument per line) in the background and wait
/// up to wait_secs for it: its output when it finishes in time, otherwise a
/// note naming the job that raj_hook_job collects later.
pub fn raj_hook_start(
    addr: impl AsRef<str>,
    token: impl AsRef<str>,
    args: impl AsRef<str>,
    wait_secs: i64,
) -> String {
    let (addr, token, args) = (addr.as_ref().to_string(), token.as_ref().to_string(), args.as_ref().to_string());
    let hook = args.split('\n').filter(|a| !a.is_empty()).nth(1).unwrap_or("").to_string();
    let job = Arc::new(Job { hook, started: Instant::now(), done: Mutex::new(None), cv: Condvar::new() });
    let id = {
        let mut g = jobs().lock().unwrap_or_else(|e| e.into_inner());
        g.0 += 1;
        let id = g.0;
        g.1.insert(id, job.clone());
        if g.1.len() > MAX_JOBS {
            let oldest = g
                .1
                .iter()
                .filter(|(_, j)| j.done.lock().map(|d| d.is_some()).unwrap_or(true))
                .map(|(k, _)| *k)
                .min();
            if let Some(k) = oldest {
                g.1.remove(&k);
            }
        }
        id
    };
    let worker = job.clone();
    std::thread::spawn(move || {
        let out = raj_hook(addr, token, args, 86_400);
        *worker.done.lock().unwrap_or_else(|e| e.into_inner()) = Some(out);
        worker.cv.notify_all();
    });
    wait_for(id, &job, wait_secs)
}

/// Wait up to wait_secs for a job started by raj_hook_start: its output once it
/// has finished, or a note that it is still running.
pub fn raj_hook_job(job: i64, wait_secs: i64) -> String {
    let found = jobs().lock().unwrap_or_else(|e| e.into_inner()).1.get(&(job as u64)).cloned();
    match found {
        Some(j) => wait_for(job as u64, &j, wait_secs),
        None => format!("error: no job {job}: jobs live only while this server runs, and only the last {MAX_JOBS} are kept"),
    }
}

fn wait_for(id: u64, job: &Job, wait_secs: i64) -> String {
    let wait = Duration::from_secs(wait_secs.clamp(0, 3_600) as u64);
    let guard = job.done.lock().unwrap_or_else(|e| e.into_inner());
    let (guard, _) = job
        .cv
        .wait_timeout_while(guard, wait, |d| d.is_none())
        .unwrap_or_else(|e| e.into_inner());
    match guard.as_ref() {
        Some(out) => out.clone(),
        None => format!(
            "running: job {id} ({}) has run for {} s; call hook_result with job {id} to wait for its output",
            job.hook,
            job.started.elapsed().as_secs()
        ),
    }
}
