//! Terminal-neutral formatting for the live activity projection.

use unicode_width::UnicodeWidthStr;

use super::ActivitySnapshot;

pub(crate) fn activity_status_text(snapshot: ActivitySnapshot, width: usize) -> String {
    if let Some(recovery) = snapshot.recovery {
        let state = if !snapshot.can_interrupt {
            "Stopping"
        } else if recovery.connecting {
            "Reconnecting"
        } else if recovery.network {
            "Waiting for network"
        } else {
            "Waiting to retry"
        };
        let detail = if recovery.connecting {
            format!("attempt {}", recovery.attempt)
        } else {
            format!("retry in {}s", recovery.remaining_seconds)
        };
        let action = if snapshot.can_interrupt {
            "esc to interrupt"
        } else {
            "stopping"
        };
        let full = format!(
            "• {state} ({} waited • {detail} • {action})",
            format_elapsed(recovery.waited_seconds)
        );
        if UnicodeWidthStr::width(full.as_str()) <= width {
            return full;
        }
        let short_action = if snapshot.can_interrupt {
            "esc"
        } else {
            "stop"
        };
        // 重连发起后不再倒计时，不能把上一轮退避秒数当作仍需等待的时间。
        let compact = if recovery.connecting {
            format!("• {state} · {short_action}")
        } else {
            format!(
                "• {state} · {}s · {short_action}",
                recovery.remaining_seconds
            )
        };
        if UnicodeWidthStr::width(compact.as_str()) <= width {
            return compact;
        }
        return truncate_end(
            if snapshot.can_interrupt {
                "• Retrying · esc"
            } else {
                "• Stopping"
            },
            width,
        );
    }
    let elapsed = format_elapsed(snapshot.elapsed.as_secs());
    let rate = snapshot.output_rate.map_or_else(
        || "--".to_owned(),
        |rate| {
            let prefix = if rate.estimated { "~" } else { "" };
            format!("{prefix}{}", format_rate(rate.tokens_per_second))
        },
    );
    let action = if snapshot.can_interrupt {
        "esc to interrupt"
    } else {
        "stopping"
    };
    let mut candidates = vec![
        format!("• {rate} tokens/s ({elapsed} • {action})"),
        format!("• {rate} tok/s ({elapsed} • {action})"),
        format!("• {rate} t/s ({elapsed} • {action})"),
        format!("• {rate} t/s • {elapsed} • {action}"),
    ];
    if snapshot.can_interrupt {
        candidates.extend([format!("• {rate} t/s • esc"), format!("• {rate} t/s")]);
    } else {
        candidates.push(format!("• {rate} t/s"));
    }

    candidates
        .iter()
        .find(|candidate| UnicodeWidthStr::width(candidate.as_str()) <= width)
        .cloned()
        .unwrap_or_else(|| truncate_end(candidates.last().expect("status candidate"), width))
}

fn format_elapsed(seconds: u64) -> String {
    if seconds < 60 {
        return format!("{seconds}s");
    }
    if seconds < 3_600 {
        return format!("{}m {:02}s", seconds / 60, seconds % 60);
    }
    format!(
        "{}h {:02}m {:02}s",
        seconds / 3_600,
        (seconds % 3_600) / 60,
        seconds % 60
    )
}

fn format_rate(rate: f64) -> String {
    if rate >= 100.0 {
        format!("{rate:.0}")
    } else {
        format!("{rate:.1}")
    }
}

fn truncate_end(value: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    if UnicodeWidthStr::width(value) <= width {
        return value.to_owned();
    }
    if width == 1 {
        return "…".to_owned();
    }
    let mut result = String::new();
    for character in value.chars() {
        let mut candidate = result.clone();
        candidate.push(character);
        if UnicodeWidthStr::width(candidate.as_str()) + 1 > width {
            break;
        }
        result.push(character);
    }
    result.push('…');
    result
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::OutputRate;

    #[test]
    fn elapsed_formats_like_codex() {
        assert_eq!(format_elapsed(0), "0s");
        assert_eq!(format_elapsed(61), "1m 01s");
        assert_eq!(format_elapsed(3_661), "1h 01m 01s");
    }

    #[test]
    fn narrow_reconnection_does_not_display_the_previous_backoff() {
        let mut snapshot = ActivitySnapshot {
            recovery: Some(crate::recovery_status::RecoverySnapshot {
                network: true,
                connecting: false,
                attempt: 2,
                remaining_seconds: 60,
                waited_seconds: 120,
            }),
            elapsed: Duration::from_secs(120),
            output_rate: None,
            can_interrupt: true,
        };
        assert_eq!(
            activity_status_text(snapshot, 40),
            "• Waiting for network · 60s · esc"
        );
        snapshot.recovery.as_mut().unwrap().connecting = true;
        assert_eq!(activity_status_text(snapshot, 40), "• Reconnecting · esc");
    }

    #[test]
    fn estimated_rates_are_explicit_and_narrow_lines_keep_escape() {
        let snapshot = ActivitySnapshot {
            recovery: None,
            elapsed: Duration::from_secs(2),
            output_rate: Some(OutputRate {
                tokens_per_second: 20.0,
                estimated: true,
            }),
            can_interrupt: true,
        };

        let line = activity_status_text(snapshot, 43);
        let compact = activity_status_text(snapshot, 20);

        assert_eq!(line, "• ~20.0 tokens/s (2s • esc to interrupt)");
        assert!(UnicodeWidthStr::width(compact.as_str()) <= 20);
        assert!(compact.contains("esc"));
    }
}
