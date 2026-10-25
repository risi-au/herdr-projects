//! Committed stage entry notifications: one coordinator prompt and one deferred relay action, with a fake clock.
use crate::common::{self, task, world};
use serde_json::json;
use sw::json::*;
use sw::store::tasks::patch;
use std::os::unix::fs::PermissionsExt;
use std::io::{Read, Write};
use std::net::TcpListener;

fn fixture() -> (common::World, String, std::path::PathBuf) {
    let w = world();
    let id = w.add("Wait for a decision");
    let socket = w.socket.clone();
    patch(&id, |t| {
        t.put("link", json!({ "socket": socket }));
        t.put("coordinator", "main");
        t.put("pr", "https://github.com/example/fiction/pull/2");
        t.put("prHead", "0123456789abcdef0123456789abcdef01234567");
    }).unwrap();
    let out = w.base.join("relay-lines");
    let relay = w.base.join("relay-mock");
    std::fs::write(&relay, format!("#!/bin/sh\nprintf '%s\\n' \"$@\" >> '{}'\n", out.display())).unwrap();
    std::fs::set_permissions(&relay, std::fs::Permissions::from_mode(0o700)).unwrap();
    unsafe { std::env::set_var("HERDR_PROJECTS_WAKE_BIN", relay); }
    (w, id, out)
}

fn enter(id: &str, stage: &str) -> i64 {
    patch(id, |t| { t.put("stage", stage); }).unwrap().at("stageNotice").i("serial")
}

#[test]
fn stage_commit_prompts_once_alerts_at_one_deferred_check_and_dedupes() {
    let (_w, id, out) = fixture();
    sw::clock::set(Some(1_000_000_000_000));
    let serial = enter(&id, "approval");
    patch(&id, |t| { t.put("msg", "repeated same-stage write"); }).unwrap();
    assert_eq!(task(&id).at("stageNotice").i("serial"), serial);
    sw::stage_notice::run(&id, serial);
    sw::stage_notice::run(&id, serial);
    let saved = task(&id);
    assert!(saved.at("stageNotice").b("pinged"));
    assert!(saved.at("stageNotice").b("alerted"));
    let text = std::fs::read_to_string(out).unwrap();
    assert_eq!(text.matches("--event").count(), 1, "relay invoked twice: {text}");
    assert!(text.contains("stage=approval") && text.contains("age=30m") && text.contains(&id));
    assert!(text.contains("/pull/2") && text.contains("0123456789abcdef0123456789abcdef01234567"));
    assert_eq!(sw::coordinator::state::get(sw::coordinator::co::Co::top(&_w.socket)).unwrap().arr("queue").len(), 1);
}

#[test]
fn stage_change_and_done_disarm_the_old_deferred_callback() {
    let (_w, id, out) = fixture();
    sw::clock::set(Some(1_000_000_000_000));
    let old = enter(&id, "asks");
    patch(&id, |t| { t.put("stage", "working"); }).unwrap();
    sw::stage_notice::run(&id, old);
    assert!(!out.exists());

    let current = enter(&id, "approval");
    assert!(current > old);
    sw::stage_notice::run(&id, old);
    assert!(!out.exists());
    patch(&id, |t| { t.put("stage", "done"); }).unwrap();
    sw::stage_notice::run(&id, current);
    assert!(!out.exists());
}

#[test]
fn leaving_and_reentering_produces_a_new_entry_and_only_the_new_one_alerts() {
    let (_w, id, out) = fixture();
    sw::clock::set(Some(1_000_000_000_000));
    let first = enter(&id, "approval");
    patch(&id, |t| t.put("stage", "working")).unwrap();
    let second = enter(&id, "approval");
    assert_eq!(second, first + 1);
    sw::stage_notice::run(&id, first);
    assert!(!out.exists());
    sw::stage_notice::run(&id, second);
    assert_eq!(std::fs::read_to_string(out).unwrap().matches("--event").count(), 1);
}

#[test]
fn failed_auth_becoming_blocked_is_a_real_entry_even_without_stage_change() {
    let (_w, id, out) = fixture();
    sw::clock::set(Some(1_000_000_000_000));
    patch(&id, |t| { t.put("stage", "failed"); t.put("failure", json!({"auth": false})); }).unwrap();
    assert_eq!(task(&id).at("stageNotice").i("serial"), 0);
    patch(&id, |t| { t.put("failure", json!({"auth": true})); }).unwrap();
    let entry = task(&id).at("stageNotice").i("serial");
    assert_eq!(entry, 1);
    patch(&id, |t| { t.put("msg", "still blocked"); }).unwrap();
    assert_eq!(task(&id).at("stageNotice").i("serial"), entry);
    sw::stage_notice::run(&id, entry);
    assert_eq!(std::fs::read_to_string(out).unwrap().matches("--event").count(), 1);
}

#[test]
fn production_relay_process_posts_to_a_local_http_sink_once_under_concurrent_callbacks() {
    let (w, id, _) = fixture();
    // A stand-in for the installed relay sends the identical argv as JSON to a local HTTP sink.
    // No secret, customer data or real relay endpoint is used.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let target = format!("http://{}/", listener.local_addr().unwrap());
    let relay = w.base.join("http-relay-mock");
    let script = format!("#!/usr/bin/env python3\nimport json,sys,urllib.request\nurllib.request.urlopen({target:?}, data=json.dumps(sys.argv[1:]).encode(), timeout=3).read()\n");
    std::fs::write(&relay, script).unwrap();
    std::fs::set_permissions(&relay, std::fs::Permissions::from_mode(0o700)).unwrap();
    unsafe { std::env::set_var("HERDR_PROJECTS_WAKE_BIN", relay); }

    let serial = enter(&id, "asks");
    let old = sw::clock::now() - sw::stage_notice::DELAY - 1000;
    patch(&id, |t| {
        let mut entry = t.at("stageNotice").clone();
        entry["entered"] = json!(old);
        t.put("stageNotice", entry);
    }).unwrap();

    let sink = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        socket.set_read_timeout(Some(std::time::Duration::from_secs(3))).unwrap();
        let mut request = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            let n = socket.read(&mut buf).unwrap();
            assert!(n > 0);
            request.extend_from_slice(&buf[..n]);
            let Some(end) = request.windows(4).position(|part| part == b"\r\n\r\n") else { continue };
            let headers = String::from_utf8_lossy(&request[..end]);
            let size = headers.lines().find_map(|line| line.to_ascii_lowercase().strip_prefix("content-length: ").and_then(|value| value.trim().parse::<usize>().ok())).unwrap();
            if request.len() < end + 4 + size { continue; }
            let posted = serde_json::from_slice::<Vec<String>>(&request[end + 4..end + 4 + size]).unwrap();
            socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nOK").unwrap();
            return posted;
        }
    });

    std::thread::scope(|scope| {
        for _ in 0..4 { scope.spawn(|| sw::stage_notice::run(&id, serial)); }
    });
    let sent = sink.join().unwrap();
    assert_eq!(&sent[..4], &["post", id.as_str(), "--event", "update"]);
    assert!(sent.join(" ").contains("stage=asks"));
    assert!(task(&id).at("stageNotice").b("alerted"));
    // Another callback has neither a second coordinator prompt nor a second HTTP POST.
    sw::stage_notice::run(&id, serial);
    assert_eq!(sw::coordinator::state::get(sw::coordinator::co::Co::top(&w.socket)).unwrap().arr("queue").len(), 1);
}

#[test]
fn the_real_task_commit_dispatches_the_production_cli_child_to_coordinator_and_http_sink() {
    let (w, id, _) = fixture();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let relay = w.base.join("real-process-http-relay");
    let script = format!("#!/usr/bin/env python3\nimport json,sys,urllib.request\nurllib.request.urlopen('http://{address}/',data=json.dumps(sys.argv[1:]).encode(),timeout=3).read()\n");
    std::fs::write(&relay, script).unwrap();
    std::fs::set_permissions(&relay, std::fs::Permissions::from_mode(0o700)).unwrap();
    unsafe {
        std::env::set_var("HERDR_PROJECTS_WAKE_BIN", relay);
        std::env::set_var("HERDR_PROJECTS_STAGE_BIN", env!("CARGO_BIN_EXE_herdr-projects"));
        std::env::set_var("HERDR_PROJECTS_INLINE", "0");
    }
    // This fixed entry time is already more than 30 minutes old in the subprocess's real clock.
    // It exercises both the post-commit dispatcher and the one-shot child without sleeping 30m.
    sw::clock::set(Some(1_000_000_000_000));
    let serial = enter(&id, "approval");
    unsafe { std::env::set_var("HERDR_PROJECTS_INLINE", "1"); }

    use std::os::fd::AsRawFd;
    let mut descriptor = libc::pollfd { fd: listener.as_raw_fd(), events: libc::POLLIN, revents: 0 };
    let ready = unsafe { libc::poll(&mut descriptor, 1, 8000) };
    assert_eq!(ready, 1, "production child did not reach isolated HTTP sink");
    let (mut stream, _) = listener.accept().unwrap();
    stream.set_read_timeout(Some(std::time::Duration::from_secs(3))).unwrap();
    let mut data = Vec::new();
    let mut buffer = [0u8; 4096];
    loop {
        let n = stream.read(&mut buffer).unwrap();
        assert!(n > 0);
        data.extend_from_slice(&buffer[..n]);
        if let Some(end) = data.windows(4).position(|part| part == b"\r\n\r\n") {
            let headers = String::from_utf8_lossy(&data[..end]);
            let size = headers.lines().find_map(|line| line.to_ascii_lowercase().strip_prefix("content-length: ").and_then(|value| value.trim().parse::<usize>().ok())).unwrap();
            if data.len() >= end + 4 + size { break; }
        }
    }
    stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nOK").unwrap();
    assert!(String::from_utf8_lossy(&data).contains("stage=approval"));
    assert_eq!(task(&id).at("stageNotice").i("serial"), serial);
    assert!(task(&id).at("stageNotice").b("alerted"));
    assert_eq!(sw::coordinator::state::get(sw::coordinator::co::Co::top(&w.socket)).unwrap().arr("queue").len(), 1);
    unsafe { std::env::remove_var("HERDR_PROJECTS_STAGE_BIN"); }
}
