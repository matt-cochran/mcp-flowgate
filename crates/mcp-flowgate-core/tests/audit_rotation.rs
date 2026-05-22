//! Tests for `FileAuditSink` date rotation and category splitting.
//!
//! Clock injection lets us control time deterministically — no sleeps.

use chrono::{DateTime, TimeZone, Utc};
use mcp_flowgate_core::audit::{AuditEvent, AuditSink, FileAuditSink, RotationInterval};
use std::sync::{Arc, Mutex};
use tempfile::TempDir;

/// Build a deterministic `FileAuditSink` whose clock is backed by a shared
/// `Arc<Mutex<DateTime<Utc>>>` so tests can advance it without sleeping.
fn make_sink_with_clock(
    dir: &TempDir,
    interval: RotationInterval,
    initial: DateTime<Utc>,
) -> (FileAuditSink, Arc<Mutex<DateTime<Utc>>>) {
    let clock_state = Arc::new(Mutex::new(initial));
    let clock_for_sink = clock_state.clone();
    let sink = FileAuditSink::with_clock(
        dir.path(),
        interval,
        Box::new(move || *clock_for_sink.lock().unwrap()),
    );
    (sink, clock_state)
}

// ---------------------------------------------------------------------------
// Test 1 — rotation: two events on different dates land in different files
// ---------------------------------------------------------------------------

#[tokio::test]
async fn file_sink_rotates_on_interval() {
    let dir = TempDir::new().unwrap();

    // Pin clock to 2026-01-15 12:00 UTC (daily rotation)
    let t1: DateTime<Utc> = Utc.with_ymd_and_hms(2026, 1, 15, 12, 0, 0).unwrap();
    let (sink, clock) = make_sink_with_clock(&dir, RotationInterval::Daily, t1);

    // Record first event at 2026-01-15
    let event1 = AuditEvent::new("workflow.started");
    sink.record(event1).await.expect("first record");

    // Advance clock to the next day
    let t2: DateTime<Utc> = Utc.with_ymd_and_hms(2026, 1, 16, 0, 5, 0).unwrap();
    *clock.lock().unwrap() = t2;

    // Record second event at 2026-01-16
    let event2 = AuditEvent::new("workflow.started");
    sink.record(event2).await.expect("second record");

    // Expect two separate audit log files
    let mut files: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| n.ends_with("-audit.log"))
        .collect();
    files.sort();

    assert_eq!(
        files.len(),
        2,
        "expected two dated audit log files, got: {:?}",
        files
    );
    assert!(
        files[0].contains("2026-01-15"),
        "first file should be for 2026-01-15, got: {}",
        files[0]
    );
    assert!(
        files[1].contains("2026-01-16"),
        "second file should be for 2026-01-16, got: {}",
        files[1]
    );
}

// ---------------------------------------------------------------------------
// Test 2 — category split: transitions go to -transitions.log; rest to -audit.log
// ---------------------------------------------------------------------------

#[tokio::test]
async fn transition_and_audit_streams_split_by_name() {
    let dir = TempDir::new().unwrap();

    // Fix clock so the stamp never changes within this test
    let fixed: DateTime<Utc> = Utc.with_ymd_and_hms(2026, 3, 10, 9, 0, 0).unwrap();
    let (sink, _clock) = make_sink_with_clock(&dir, RotationInterval::Daily, fixed);
    let stamp = "2026-03-10";

    // Record a workflow.transition event (goes to transitions log)
    let transition_event = AuditEvent::new("workflow.transition");
    sink.record(transition_event).await.expect("transition record");

    // Record an unrelated event (goes to audit log)
    let audit_event = AuditEvent::new("workflow.started");
    sink.record(audit_event).await.expect("audit record");

    let transitions_file = dir.path().join(format!("{stamp}-transitions.log"));
    let audit_file = dir.path().join(format!("{stamp}-audit.log"));

    assert!(
        transitions_file.exists(),
        "transitions log should exist at {:?}",
        transitions_file
    );
    assert!(
        audit_file.exists(),
        "audit log should exist at {:?}",
        audit_file
    );

    // Verify content: transitions log has exactly one line (the transition event)
    let trans_content = std::fs::read_to_string(&transitions_file).unwrap();
    let trans_lines: Vec<&str> = trans_content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .collect();
    assert_eq!(
        trans_lines.len(),
        1,
        "transitions log should have exactly one event"
    );
    let trans_event: serde_json::Value = serde_json::from_str(trans_lines[0]).unwrap();
    assert_eq!(
        trans_event["event_type"],
        "workflow.transition",
        "transitions log should contain the transition event"
    );

    // Verify content: audit log has exactly one line (the non-transition event)
    let audit_content = std::fs::read_to_string(&audit_file).unwrap();
    let audit_lines: Vec<&str> = audit_content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .collect();
    assert_eq!(
        audit_lines.len(),
        1,
        "audit log should have exactly one event"
    );
    let audit_parsed: serde_json::Value = serde_json::from_str(audit_lines[0]).unwrap();
    assert_eq!(
        audit_parsed["event_type"],
        "workflow.started",
        "audit log should contain the non-transition event"
    );
}

// ---------------------------------------------------------------------------
// Test 3 — hourly rotation stamp format
// ---------------------------------------------------------------------------

#[tokio::test]
async fn hourly_rotation_uses_hour_stamp() {
    let dir = TempDir::new().unwrap();

    let t1: DateTime<Utc> = Utc.with_ymd_and_hms(2026, 6, 1, 14, 30, 0).unwrap();
    let (sink, clock) = make_sink_with_clock(&dir, RotationInterval::Hourly, t1);

    sink.record(AuditEvent::new("workflow.started"))
        .await
        .unwrap();

    // Advance past the hour boundary
    let t2: DateTime<Utc> = Utc.with_ymd_and_hms(2026, 6, 1, 15, 2, 0).unwrap();
    *clock.lock().unwrap() = t2;

    sink.record(AuditEvent::new("workflow.started"))
        .await
        .unwrap();

    let mut files: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| n.ends_with("-audit.log"))
        .collect();
    files.sort();

    assert_eq!(
        files.len(),
        2,
        "hourly rotation should produce two files, got: {:?}",
        files
    );
    assert!(
        files[0].contains("2026-06-01-14"),
        "first file should contain hour 14, got: {}",
        files[0]
    );
    assert!(
        files[1].contains("2026-06-01-15"),
        "second file should contain hour 15, got: {}",
        files[1]
    );
}

// ---------------------------------------------------------------------------
// Test 4 — weekly rotation stamp format
// ---------------------------------------------------------------------------

#[tokio::test]
async fn weekly_rotation_uses_iso_week_stamp() {
    let dir = TempDir::new().unwrap();

    let t1: DateTime<Utc> = Utc.with_ymd_and_hms(2026, 1, 12, 10, 0, 0).unwrap();
    let (sink, clock) = make_sink_with_clock(&dir, RotationInterval::Weekly, t1);

    sink.record(AuditEvent::new("workflow.started"))
        .await
        .unwrap();

    // Advance to a date in a different ISO week (more than 10 days apart)
    let t2: DateTime<Utc> = Utc.with_ymd_and_hms(2026, 1, 26, 10, 0, 0).unwrap();
    *clock.lock().unwrap() = t2;

    sink.record(AuditEvent::new("workflow.started"))
        .await
        .unwrap();

    let mut files: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| n.ends_with("-audit.log"))
        .collect();
    files.sort();

    assert_eq!(
        files.len(),
        2,
        "weekly rotation should produce two files, got: {:?}",
        files
    );
    assert!(
        files[0].contains("2026-W03"),
        "first file should contain ISO week 2026-W03, got: {}",
        files[0]
    );
    assert!(
        files[1].contains("2026-W05"),
        "second file should contain ISO week 2026-W05, got: {}",
        files[1]
    );
}
