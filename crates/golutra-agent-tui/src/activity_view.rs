//! Terminal-neutral formatting for the live activity projection.

use unicode_width::UnicodeWidthStr;

use super::ActivitySnapshot;

pub(crate) fn activity_status_text(snapshot: ActivitySnapshot, width: usize) -> String {
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
    fn estimated_rates_are_explicit_and_narrow_lines_keep_escape() {
        let snapshot = ActivitySnapshot {
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
