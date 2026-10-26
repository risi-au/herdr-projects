//! Start one detached, one-shot stage-entry delivery job after the task's committed stage change.
//! The task record supplies the durable entry sequence; job delivery claims live in the same record.
use crate::json::*;
use super::files::{bin, data_dir};
use std::process::{Command, Stdio};

/// The persisted stages whose board state waits for a human. Some blocked states are expressed
/// by a field on the same stage, so compare this before and after every atomic task commit.
pub fn entry_stage(t: &Obj) -> Option<&str> {
    match t.s("stage") {
        "approval" | "asks" | "reviewneeds" | "plan" | "pick" => Some(t.s("stage")),
        "failed" if t.at("failure").b("auth") => Some("failed"),
        "stopped" if t.b("limitStop") => Some("stopped"),
        "after" if t.b("afterBlocked") => Some("after"),
        _ => None,
    }
}

pub fn dispatch_once(t: &Obj) {
    if std::env::var("HERDR_PROJECTS_INLINE").as_deref() == Ok("1") { return; }
    // An isolated integration test can choose the built production binary instead of the test harness.
    let program = std::env::var("HERDR_PROJECTS_STAGE_BIN").map(std::path::PathBuf::from).unwrap_or_else(|_| bin());
    let mut child = Command::new(program);
    child.args(["run", "stage-notice", t.s("id"), &t.at("stageNotice").i("serial").to_string()])
        .current_dir(data_dir()).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    use std::os::unix::process::CommandExt;
    unsafe { child.pre_exec(|| { libc::setsid(); Ok(()) }); }
    if let Ok(mut process) = child.spawn() {
        let _ = std::thread::Builder::new().name("notice-reap".into()).spawn(move || { let _ = process.wait(); });
    }
}
