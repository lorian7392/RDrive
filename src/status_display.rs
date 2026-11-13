//! Compact human output for the source-labelled status snapshot. JSON remains
//! the complete interface; unavailable measurements are never displayed as zero.
use anyhow::{Context, Result};
use serde_json::Value;
use std::io::Write;

fn clean(value: &str) -> String {
    value
        .chars()
        .take(512)
        .map(|c| {
            if c.is_control() {
                format!("\\u{{{:x}}}", c as u32)
            } else {
                c.to_string()
            }
        })
        .collect()
}
fn text(value: &Value) -> String {
    match value {
        Value::Null => "unknown".into(),
        Value::String(value) => clean(value),
        other => clean(&other.to_string()),
    }
}
fn bytes(value: &Value) -> String {
    let Some(size) = value.as_u64() else {
        return "unknown".into();
    };
    let mut size = size as f64;
    let mut unit = 0;
    let units = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    while size >= 1024.0 && unit + 1 < units.len() {
        size /= 1024.0;
        unit += 1;
    }
    format!("{size:.1} {}", units[unit])
}
fn budget(value: Option<&Value>) -> String {
    match value {
        Some(Value::Null) => "unlimited".into(),
        Some(value) => bytes(value),
        None => "unknown".into(),
    }
}
fn time(value: &Value, millis: bool) -> String {
    if value.is_string() {
        return text(value);
    }
    let Some(number) = value.as_i64() else {
        return "unknown".into();
    };
    let date = if millis {
        chrono::DateTime::<chrono::Utc>::from_timestamp_millis(number)
    } else {
        chrono::DateTime::from_timestamp(number, 0)
    };
    date.map(|date| date.to_rfc3339())
        .unwrap_or_else(|| "unknown".into())
}
fn message(value: &Value) -> String {
    let detail = if value.get("detail").is_some() {
        &value["detail"]
    } else {
        value
    };
    if let Some(value) = detail.get("message") {
        text(value)
    } else if let Some(value) = detail.get("code") {
        text(value)
    } else {
        text(detail)
    }
}
fn line(output: &mut String, value: impl AsRef<str>) {
    output.push_str(value.as_ref());
    output.push('\n');
}
fn header(output: &mut String, title: &str, section: &Value) -> bool {
    line(output, format!("\n{title} [{}]", text(&section["status"])));
    if let Some(error) = section.get("error") {
        line(output, format!("  {}", message(error)));
    }
    if !section["probe_error"].is_null() {
        line(
            output,
            format!(
                "  Live check did not finish: {}",
                message(&section["probe_error"])
            ),
        );
    }
    !section["data"].is_null()
}
fn rows<'a>(value: &'a Value) -> impl Iterator<Item = &'a Value> {
    value.as_array().into_iter().flatten()
}
fn errors(output: &mut String, data: &Value) {
    if data["error_components"]
        .as_u64()
        .is_some_and(|count| count > 0)
    {
        line(
            output,
            format!(
                "  Unavailable components: {}",
                text(&data["error_components"])
            ),
        );
    }
    for key in ["profiles", "domains"] {
        for row in rows(&data[key]) {
            if let Some(error) = row.get("error") {
                line(
                    output,
                    format!(
                        "  {}: {}",
                        text(row.get("profile").unwrap_or(&row["domain"])),
                        message(error)
                    ),
                );
            }
        }
    }
}

pub fn render(report: &Value, details: bool) -> String {
    let mut output = String::new();
    line(
        &mut output,
        format!("RDrive {} status", text(&report["version"])),
    );
    line(
        &mut output,
        format!("State directory: {}", text(&report["state_directory"])),
    );
    line(
        &mut output,
        format!("Observed: {}", text(&report["observed_at"])),
    );
    if header(&mut output, "Connections", &report["connections"]) {
        let data = &report["connections"]["data"];
        if rows(&data["profiles"]).next().is_none() {
            line(&mut output, "  No current connection observations.");
        }
        for profile in rows(&data["profiles"]) {
            line(
                &mut output,
                format!(
                    "  {}: {} | checked {}",
                    text(&profile["profile"]),
                    text(&profile["state"]),
                    time(&profile["checked_at"], false)
                ),
            );
            line(
                &mut output,
                format!(
                    "    Last success: {}",
                    time(&profile["last_success_at"], false)
                ),
            );
            if !profile["last_error"].is_null() {
                line(
                    &mut output,
                    format!(
                        "    Last error: {} at {}",
                        message(&profile["last_error"]),
                        time(&profile["last_error"]["at"], false)
                    ),
                );
            }
            for key in ["probe_error", "observation_error"] {
                if !profile[key].is_null() {
                    line(
                        &mut output,
                        format!("    {key}: {}", message(&profile[key])),
                    );
                }
            }
            if details {
                line(
                    &mut output,
                    format!(
                        "    Evidence: {} | freshness {} seconds",
                        text(&profile["source"]),
                        text(&profile["freshness_seconds"])
                    ),
                );
            }
        }
        if data["configuration_changed"] == true {
            line(
                &mut output,
                "  Connection settings changed during inspection; repeat the command.",
            );
        }
    }
    if header(&mut output, "Background service", &report["service"]) {
        let data = &report["service"]["data"];
        line(
            &mut output,
            format!(
                "  Installed: {} | supervisor: {} | heartbeat stale: {}",
                text(&data["installed"]),
                text(&data["supervisor"]["state"]),
                text(&data["heartbeat_stale"])
            ),
        );
        line(
            &mut output,
            format!(
                "  Service running verified: {} | supervisor lease held: {}",
                text(&data["supervisor_running_observed"]),
                text(&data["supervisor_lease_held"])
            ),
        );
        for role in ["broker", "native_watcher"] {
            line(
                &mut output,
                format!(
                    "  {role}: {} | PID {}",
                    text(&data["children"][role]["state"]),
                    text(&data["children"][role]["pid"])
                ),
            );
        }
    }
    if header(&mut output, "Directory index", &report["index"]) {
        let data = &report["index"]["data"];
        for domain in rows(&data["domains"]) {
            line(
                &mut output,
                format!(
                    "  {}: initialized {} | {} items | {} scanned directories | {} pending",
                    text(&domain["domain"]),
                    text(&domain["initialized"]),
                    text(&domain["items"]),
                    text(&domain["scanned"]),
                    text(&domain["pending"])
                ),
            );
            line(
                &mut output,
                format!(
                    "    Last directory refresh: {}",
                    time(&domain["last_success_at_ms"], true)
                ),
            );
        }
        errors(&mut output, data);
    }
    if header(&mut output, "Cache", &report["cache"]) {
        let data = &report["cache"]["data"];
        line(
            &mut output,
            format!(
                "  Global accounted use: {} | reservations: {}",
                bytes(&data["accounted_bytes"]),
                bytes(&data["reserved_bytes"])
            ),
        );
        line(
            &mut output,
            format!(
                "  Global budget: {} | Rust cache in scope: {} / {} files",
                budget(data.get("max_bytes")),
                bytes(&data["indexed_bytes"]),
                text(&data["indexed_files"])
            ),
        );
        line(
            &mut output,
            format!(
                "  Rust cache ledger recorded: {}",
                text(&data["rust_ledger_present"])
            ),
        );
        line(
            &mut output,
            format!(
                "  Pinned Rust contents: {} / {} files",
                bytes(&data["pinned_bytes"]),
                text(&data["pinned_files"])
            ),
        );
        line(
            &mut output,
            format!(
                "  Native reported usage (global): {} | reserved: {} | as of {}",
                bytes(&data["native"]["used_bytes"]),
                bytes(&data["native"]["reserved_bytes"]),
                time(&data["native"]["reported_at"], false)
            ),
        );
        line(
            &mut output,
            format!(
                "  Over budget: {} | missing indexed files: {} | unbound entries: {}",
                text(&data["over_budget"]),
                text(&data["missing_files"]),
                text(&data["unbound_entries"])
            ),
        );
        line(
            &mut output,
            format!("  Clean cache directory: {}", text(&data["directory"])),
        );
    }
    if header(&mut output, "File history", &report["history"]) {
        let data = &report["history"]["data"];
        for profile in rows(&data["profiles"]) {
            line(
                &mut output,
                format!(
                    "  {}: enabled {} | {} entries | {} / {} logical budget",
                    text(&profile["profile"]),
                    text(&profile["policy"]["enabled"]),
                    text(&profile["entries"]),
                    bytes(&profile["bytes"]),
                    budget(profile["policy"].get("max_bytes"))
                ),
            );
            line(
                &mut output,
                format!(
                    "    Keep {} versions / {} days | protected {} | latest {}",
                    text(&profile["policy"]["keep"]),
                    text(&profile["policy"]["max_age_days"]),
                    text(&profile["prepared"]),
                    time(&profile["latest_created_at"], false)
                ),
            );
        }
        errors(&mut output, data);
    }
    if header(&mut output, "APFS checkpoints", &report["checkpoints"]) {
        let data = &report["checkpoints"]["data"];
        line(
            &mut output,
            format!(
                "  Enabled: {} | storage: {} | record target: {}",
                text(&data["policy"]["enabled"]),
                text(&data["policy"]["storage"]),
                text(&data["policy"]["max_records"])
            ),
        );
        line(
            &mut output,
            format!(
                "  Fixed image: {} | allocated on host: {} | initialized (recorded): {}",
                bytes(&data["fixed_image_bytes"]),
                bytes(&data["host_allocated_bytes"]),
                text(&data["identity_ready"])
            ),
        );
        line(
            &mut output,
            format!(
                "  Last confirmed: {} at {}",
                text(&data["last_confirmed_checkpoint"]["checkpoint_id"]),
                time(&data["last_confirmed_checkpoint"]["completed_at"], false)
            ),
        );
        line(
            &mut output,
            format!(
                "  Next scheduled: {} | retry: {}",
                time(&data["next_due_at"], false),
                time(&data["schedule"]["retry_at"], false)
            ),
        );
        if !data["schedule"]["last_error"].is_null() {
            line(
                &mut output,
                format!(
                    "  Scheduler error: {}",
                    message(&data["schedule"]["last_error"])
                ),
            );
        }
        line(
            &mut output,
            format!(
                "  Schedule failures: {} | last deferred: {}",
                text(&data["schedule"]["failure_count"]),
                text(&data["schedule"]["last_deferred"])
            ),
        );
        let verified = &report["checkpoints"]["verification"];
        if verified.is_null() {
            line(&mut output,"  Internal usage/count not checked; use --check-checkpoints for a read-only inspection.");
        } else {
            line(
                &mut output,
                format!(
                    "  Store inspection: {} | used {} | records {}",
                    text(&verified["status"]),
                    bytes(&verified["data"]["used_bytes"]),
                    text(
                        verified["data"]
                            .get("records")
                            .unwrap_or(&verified["data"]["local_record_count"])
                    )
                ),
            );
            if !verified["data"]["system_snapshot_count"].is_null() {
                line(
                    &mut output,
                    format!(
                        "  Independent system snapshots: {}",
                        text(&verified["data"]["system_snapshot_count"])
                    ),
                );
            }
            if let Some(error) = verified.get("error") {
                line(&mut output, format!("    {}", message(error)));
            }
        }
    }
    if header(
        &mut output,
        "Conflicts and retained edits",
        &report["conflicts"],
    ) {
        let data = &report["conflicts"]["data"];
        line(
            &mut output,
            format!(
                "  Synchronization paused: {} | Smart edits needing attention: {}",
                text(&data["control"]["paused"]),
                text(&data["needs_attention"])
            ),
        );
        for domain in rows(&data["domains"]) {
            line(
                &mut output,
                format!("  {}: {}", text(&domain["domain"]), text(&domain["counts"])),
            );
        }
        if details {
            for item in rows(&data["examples"]) {
                line(
                    &mut output,
                    format!(
                        "    {} {} | {} | path unknown: {} | {}",
                        text(&item["state"]),
                        text(&item["mutation"]),
                        text(
                            item.get("path")
                                .filter(|v| !v.is_null())
                                .or_else(|| item.get("name").filter(|v| !v.is_null()))
                                .unwrap_or(&item["item_id"])
                        ),
                        text(&item["source_path_unknown"]),
                        message(&item["error"])
                    ),
                );
            }
        }
        if data["unbound_journal_directories"]
            .as_u64()
            .is_some_and(|count| count > 0)
        {
            line(
                &mut output,
                format!(
                    "  Unbound Smart journals (global): {} | {}",
                    text(&data["unbound_journal_directories"]),
                    text(&data["unbound_journal_state"])
                ),
            );
        }
        line(&mut output,format!("  Ordinary sync plans: {} (last observations; re-plan to verify current conflicts)",text(&data["ordinary_sync"]["status"])));
        for plan in rows(&data["ordinary_sync"]["plans"]) {
            line(
                &mut output,
                format!(
                    "    {}: {} conflicts | checked {}",
                    text(&plan["profile"]),
                    text(&plan["conflict_count"]),
                    time(&plan["observed_at"], false)
                ),
            );
        }
        errors(&mut output, data);
    }
    if header(&mut output, "Upload queue", &report["queue"]) {
        let data = &report["queue"]["data"];
        line(
            &mut output,
            format!(
                "  {} jobs | states {} | retained contents {}",
                text(&data["total"]),
                text(&data["counts"]),
                bytes(&data["retained_bytes"])
            ),
        );
        if details {
            for item in rows(&data["examples"]) {
                line(
                    &mut output,
                    format!(
                        "    {}: {} | {}",
                        text(&item["id"]),
                        text(&item["state"]),
                        message(&item["error"])
                    ),
                );
            }
        }
    }
    if header(&mut output, "Resumable transfers", &report["transfers"]) {
        let data = &report["transfers"]["data"];
        line(
            &mut output,
            format!(
                "  {} retained transfer journals | states {}",
                text(&data["total"]),
                text(&data["counts"])
            ),
        );
        if details {
            for item in rows(&data["examples"]) {
                line(
                    &mut output,
                    format!(
                        "    {}: {} | {} / {} verified bytes",
                        text(&item["id"]),
                        text(&item["phase"]),
                        text(&item["verified_bytes"]),
                        text(&item["total_bytes"])
                    ),
                );
            }
        }
    }
    if header(&mut output, "Configuration", &report["configuration"]) {
        line(
            &mut output,
            format!(
                "  Selected profile: {} | {} configured profiles",
                if report["selected_profile"].is_null() {
                    "all".into()
                } else {
                    text(&report["selected_profile"])
                },
                text(&report["configuration"]["data"]["configured_profiles"])
            ),
        );
        if details {
            let formatted =
                serde_json::to_string_pretty(&report["configuration"]["data"]).unwrap_or_default();
            for row in formatted.lines() {
                line(&mut output, format!("  {row}"));
            }
        }
    }
    line(&mut output,"\nConnection state is timestamped evidence. Use --check-connections for fresh authenticated checks; --json exposes every observation and partial error.");
    output
}

pub fn write(report: &Value, details: bool, output: &mut impl Write) -> Result<()> {
    output
        .write_all(render(report, details).as_bytes())
        .context("cannot write status output")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn incomplete_snapshots_are_explicit_and_terminal_controls_are_escaped() {
        let report = json!({"version":"test","state_directory":"state\u{1b}[31m\nprivate","observed_at":"now","connections":{"status":"error","error":{"detail":{"message":"unavailable"}},"probe_error":{"message":"probe interrupted"}}});
        let result = render(&report, false);
        assert!(
            result.contains("unknown")
                && result.contains("Connections [error]")
                && result.contains("unavailable")
        );
        assert!(!result.contains('\u{1b}') && result.contains("\\u{1b}"));
        assert_eq!(bytes(&Value::Null), "unknown");
        assert_eq!(bytes(&json!(1024)), "1.0 KiB");
        assert_eq!(bytes(&json!(u64::MAX)), "16384.0 PiB");
        assert_eq!(time(&json!(i64::MAX), false), "unknown");
        assert!(time(&json!(0), true).contains("1970"));
        assert_eq!(message(&json!({"code":"busy"})), "busy");
        assert_eq!(message(&json!("retained issue")), "retained issue");
        assert_eq!(budget(Some(&Value::Null)), "unlimited");
        assert_eq!(budget(None), "unknown");
        assert_eq!(budget(Some(&json!(1024))), "1.0 KiB");
        assert!(
            render(&json!({"configuration":{"status":"ok","data":{}}}), false)
                .contains("Selected profile: all")
        );
    }
    #[test]
    fn complete_summary_keeps_scopes_timestamps_details_and_error_reasons() {
        let data = json!({"profiles":[{"profile":"nas","state":"online","checked_at":"now","last_success_at":"now","last_error":{"detail":{"message":"TLS verification failed"}},"probe_error":{"message":"explicit probe failed"},"observation_error":{"message":"record could not be saved"},"error":{"message":"history unavailable"},"source":"probe","freshness_seconds":300,"policy":{"enabled":true,"max_bytes":1024,"keep":20,"max_age_days":30},"entries":3,"bytes":4,"prepared":1,"latest_created_at":"now"}],"configuration_changed":true,"installed":true,"supervisor":{"state":"running"},"children":{"broker":{"state":"running","pid":123},"native_watcher":{"state":"running","pid":124}},"heartbeat_stale":false,"domains":[{"domain":"nas-smart","items":20,"scanned":2,"pending":3,"last_success_at_ms":0,"error":{"message":"busy"},"counts":{"conflict":1}}],"max_bytes":2048,"indexed_bytes":1024,"indexed_files":1,"pinned_bytes":1024,"pinned_files":1,"native":{"used_bytes":20,"reserved_bytes":10},"directory":"private-cache","policy":{"enabled":true,"storage":"apfs_vault","max_records":48},"fixed_image_bytes":21474836480_u64,"host_allocated_bytes":21474836480_u64,"identity_ready":true,"last_confirmed_checkpoint":{"checkpoint_id":"point","completed_at":0},"next_due_at":3600,"schedule":{"retry_at":null,"last_error":{"message":"not enough space"}},"control":{"paused":false},"needs_attention":1,"ordinary_sync":{"status":"last_observed","plans":[{"profile":"nas","conflict_count":1,"observed_at":"now"}]},"counts":{"failed":1},"total":1,"retained_bytes":32,"examples":[{"state":"conflict","mutation":"mutation","path":"book.txt","error":{"message":"changed"},"id":"job","phase":"receiving","verified_bytes":1,"total_bytes":4}],"error_components":1,"configured_profiles":2,"supervisor_running_observed":false,"supervisor_lease_held":false,"unbound_journal_directories":1,"unbound_journal_state":"unknown account"});
        let mut report = json!({"version":"test","state_directory":"state","observed_at":"now","profiles":1,"selected_profile":"nas"});
        for name in [
            "connections",
            "service",
            "index",
            "cache",
            "history",
            "checkpoints",
            "conflicts",
            "queue",
            "transfers",
            "configuration",
        ] {
            report[name] = json!({"status":"ok","data":data});
        }
        let summary = render(&report, false);
        let detailed = render(&report, true);
        assert!(
            summary.contains("Last success: now")
                && summary.contains("TLS verification failed")
                && summary.contains("20.0 GiB")
                && summary.contains("Unavailable components: 1")
                && summary.contains("history unavailable")
                && summary.contains("record could not be saved")
                && summary.contains("explicit probe failed")
                && summary.contains("2 configured profiles")
                && summary.contains("Service running verified: false")
                && summary.contains("Unbound Smart journals (global): 1")
        );
        assert!(
            !summary.contains("book.txt")
                && detailed.contains("book.txt")
                && detailed.contains("Evidence: probe")
        );
        assert!(
            detailed.contains("retained transfer journals")
                && detailed.contains("not enough space")
        );
        report["checkpoints"]["verification"] = json!({"status":"error","error":{"message":"image busy"},"data":{"used_bytes":1024,"records":1}});
        assert!(render(&report, false).contains("image busy"));
        report["checkpoints"]["verification"] =
            json!({"status":"ok","data":{"used_bytes":1024,"records":1}});
        assert!(render(&report, false).contains("records 1"));
        report["checkpoints"]["verification"] =
            json!({"status":"ok","data":{"local_record_count":2,"system_snapshot_count":13}});
        assert!(
            render(&report, false).contains("records 2")
                && render(&report, false).contains("Independent system snapshots: 13")
        );
        report["conflicts"]["data"]["examples"][0]["path"] = Value::Null;
        report["conflicts"]["data"]["examples"][0]["name"] = json!("retained-name.txt");
        assert!(render(&report, true).contains("retained-name.txt"));
        report["conflicts"]["data"]["examples"][0]["name"] = Value::Null;
        report["conflicts"]["data"]["examples"][0]["item_id"] = json!("retained-id");
        assert!(render(&report, true).contains("retained-id"));
        let mut output = Vec::new();
        write(&report, true, &mut output).unwrap();
        assert_eq!(String::from_utf8(output).unwrap(), render(&report, true));
        struct Broken;
        impl Write for Broken {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(std::io::ErrorKind::BrokenPipe.into())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        assert!(write(&report, false, &mut Broken)
            .unwrap_err()
            .to_string()
            .contains("status output"));
    }
}
