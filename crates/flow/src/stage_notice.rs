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
        if serial <= 0 || entry.i("serial") != serial || entry.s("stage") != t.s("stage")
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

fn metadata(t: &Obj) -> (String, String, String) {
    let pr = t.t("pr");
    let head = [t.s("prHead"), t.s("reviewedHead"), t.at("kept").s("head"), t.s("checkedHead")]
        .into_iter().find(|s| !s.is_empty()).unwrap_or("unknown").to_string();
    let merge = if t.s("stage") != "approval" {
        format!("blocked: coordinator to resolve according to the recorded kind ({})", if t.b("kind") { t.s("kind") } else { "unspecified" })
    } else if t.b("requireApproval") || !crate::finish::auto_merge::by_itself(t) {
        if t.s("owner") == crate::store::files::me() { "needs Rishi's own approval under the task's merge policy".into() }
        else { "requires the assigned owner's approval under the task's merge policy".into() }
    } else { "merge.by=itself: coordinator checks existing review and merge gates".into() };
    (pr, head, merge)
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
    let Some(co) = owner(t) else { return; };
    let (pr, head, merge) = metadata(t);
    let message = format!("Task {} entered {} (entry {}). PR: {}; head SHA: {}. {}. Inspect the task and coordinate the next action within existing approval rules.",
        t.s("id"), t.s("stage"), t.at("stageNotice").i("serial"), if pr.is_empty() { "unknown" } else { &pr }, head, merge);
    let _ = requests::send(&folder_of(t.s("id")), &message, false, t.s("id"), co);
}

fn alert(t: &Obj) {
    let (pr, head, merge) = metadata(t);
    let entry = t.at("stageNotice");
    let age = now().saturating_sub(entry.i("entered")) / 60000;
    let bin = std::env::var("HERDR_PROJECTS_WAKE_BIN").unwrap_or_else(|_| "/home/rishi/.local/bin/herdr-board-wake".into());
    // This existing relay owns its HTTP endpoint/config. Arguments are never interpreted by a shell.
    let detail = format!("task={} stage={} entry={} age={}m PR={} head={} {}", t.s("id"), t.s("stage"), entry.i("serial"), age,
        if pr.is_empty() { "unknown" } else { &pr }, head, merge);
    let _ = Command::new(bin).args(["post", t.s("id"), "--event", "update", "--msg", &detail])
        .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status();
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
