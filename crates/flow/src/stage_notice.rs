//! An actual committed approval/blocked stage entry prompts its owning coordinator once, then checks it once 30m
//! later. Claims are persisted before external delivery: a crash can lose a notification but never duplicate it.
use crate::clock::{now, pause};
use crate::json::*;
use crate::store::tasks::{get, update, Change};
use crate::store::repos::folder_of;
use crate::coordinator::{co::Co, requests};
use std::process::{Command, Stdio};

pub const DELAY: i64 = 30 * 60 * 1000;

fn claim(id: &str, serial: i64, column: &str, due: bool) -> Option<Obj> {
    let mut granted = false;
    let t = update(id, |t| {
        let entry = t.at("stageNotice");
        if serial <= 0 || entry.i("serial") != serial || !entry.b("active")
            || crate::store::stage_notice::entry_stage(t).is_none_or(|s| s != entry.s("stage"))
            || t.b("archivedAt") || t.s("stage") == "done" || entry.b(column)
            || (due && now().saturating_sub(entry.i("entered")) < DELAY) { return Ok(Change::Keep); }
        let mut e = entry.clone();
        if let Some(o) = e.as_object_mut() { o.insert(column.into(), json!(true)); }
        t.put("stageNotice", e);
        granted = true;
        Ok(Change::Write)
    }).ok()?;
    granted.then_some(t)
}

pub fn approval_class(t: &Obj) -> String {
    if t.s("stage") != "approval" { return "UNKNOWN: blocked state requires coordinator investigation".into(); }
    // The stored task has no named human approver. `owner`, `merge.by=you`, and
    // `requireApproval` refer to board workflow, not to Rishi's authority.
    if t.b("requireApproval") { return "UNKNOWN: task requires a human decision; the approver is not recorded".into(); }
    if t.b("syncBlocked") || t.b("mergeFailed") || t.b("failure") || t.b("prFailure") {
        return "UNKNOWN: merge is blocked; coordinator must inspect the recorded failure".into();
    }
    if !crate::finish::auto_merge::by_itself(t) {
        return "UNKNOWN: merge.by=you or a held merge requires approval; no named Rishi authority is recorded".into();
    }
    if !crate::finish::auto_merge::green(t) {
        return "UNKNOWN: automatic merge gates are not confirmed (review, head, checks, or task state)".into();
    }
    "NORMAL: existing automatic merge path is eligible; coordinator verifies current gates before merge".into()
}

fn metadata(t: &Obj) -> (String, String, String) {
    let pr = t.t("pr");
    let head = [t.s("prHead"), t.s("reviewedHead"), t.at("kept").s("head"), t.s("checkedHead")]
        .into_iter().find(|s| !s.is_empty()).unwrap_or("unknown").to_string();
    (pr, head, approval_class(t))
}

fn owner(t: &Obj) -> Option<Co> {
    let socket = t.at("link").s("socket");
    if socket.is_empty() { return None; }
    let explicit = t.s("coordinator");
    if !explicit.is_empty() { return Some(Co::of(socket, if explicit == "main" { "" } else { explicit })); }
    // The original human-created task belongs to the main coordinator in the task's actual herdr session.
    (t.s("owner") == crate::store::files::me()).then(|| Co::top(socket))
}

fn prompt(t: &Obj) {
    let Some(co) = owner(t) else { eprintln!("stage-notice: coordinator route unavailable"); return; };
    let (pr, head, merge) = metadata(t);
    let message = format!("Task {} entered {} (entry {}). PR: {}; head SHA: {}. {}. Inspect the task and coordinate the next action within existing approval rules.",
        t.s("id"), t.s("stage"), t.at("stageNotice").i("serial"), if pr.is_empty() { "unknown" } else { &pr }, head, merge);
    if requests::send(&folder_of(t.s("id")), &message, false, t.s("id"), co).is_err() {
        eprintln!("stage-notice: coordinator message enqueue failed");
    }
}

fn alert(t: &Obj) {
    let (pr, head, merge) = metadata(t);
    let entry = t.at("stageNotice");
    let age = now().saturating_sub(entry.i("entered")) / 60000;
    let detail = format!("task={} stage={} entry={} age={}m PR={} head={} approval={} ", t.s("id"), t.s("stage"), entry.i("serial"), age,
        if pr.is_empty() { "unknown" } else { &pr }, head, merge);
    // The installed board-wake `post` writes cos-status.json; it does NOT post to Omarchy.
    // Resolve through the same owner-routing client as the board's existing blocked-task flow.
    let route = std::env::var("HERDR_PROJECTS_ALERT_ROUTE_BIN")
        .unwrap_or_else(|_| "/home/rishi/.local/bin/herdr-stuck-alert".into());
    let resolved = Command::new(route).args(["--who", &format!("task:{}", t.s("id")), "--task", t.s("id")])
        .stdin(Stdio::null()).stderr(Stdio::null()).output();
    let Some(to) = resolved.ok().filter(|o| o.status.success()).and_then(|o| {
        let line = std::str::from_utf8(&o.stdout).ok()?.trim();
        let who = line.strip_prefix("to=")?.split_once(" why=")?.0;
        (!who.is_empty() && who.len() <= 120 && !who.starts_with('-')
            && !who.chars().any(char::is_control)).then(|| who.to_string())
    }) else { eprintln!("stage-notice: Omarchy owner route unavailable"); return; };
    // ask-grokbot is the actual installed Omarchy HTTPS POST client. It owns endpoint/key
    // loading and never places a key on argv. Disable its internal retries and fallback:
    // the persisted task claim permits exactly one HTTP attempt for this entry.
    let bin = std::env::var("HERDR_PROJECTS_ALERT_BIN")
        .unwrap_or_else(|_| "/home/rishi/.local/bin/ask-grokbot".into());
    match Command::new(bin).args([&to, &detail]).env("GROKBOT_NO_FALLBACK", "1")
        .env("GROKBOT_RETRY_DELAYS", " ")
        .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status() {
        Ok(status) if status.success() => {},
        Ok(status) => eprintln!("stage-notice: Omarchy POST attempt failed (exit={})", status.code().unwrap_or(-1)),
        Err(_) => eprintln!("stage-notice: Omarchy POST client unavailable"),
    }
}

/// Runs in one detached child per stage entry. The child sleeps once and exits; no poll or timer service exists.
pub fn run(id: &str, serial: i64) {
    if let Some(t) = claim(id, serial, "pinged", false) { prompt(&t); }
    if let Some(t) = get(id) {
        if t.at("stageNotice").i("serial") != serial { return; }
        let left = DELAY.saturating_sub(now().saturating_sub(t.at("stageNotice").i("entered")));
        if left > 0 { pause(left as u64); }
    }
    if let Some(t) = claim(id, serial, "alerted", true) { alert(&t); }
}
