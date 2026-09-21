//! 可选的 UI 阶段计时：只记录阶段和耗时，热路径不写磁盘、不保存正文或按键。

use std::{
    collections::VecDeque,
    path::PathBuf,
    sync::{Mutex, OnceLock},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

const SAMPLE_LIMIT: usize = 16_384;

struct Trace {
    path: PathBuf,
    origin: Instant,
    epoch_us: u128,
    samples: Mutex<VecDeque<(&'static str, u128, u128)>>,
}

fn trace() -> Option<&'static Trace> {
    static TRACE: OnceLock<Option<Trace>> = OnceLock::new();
    TRACE
        .get_or_init(|| {
            std::env::var_os("GOLUTRA_AGENT_TUI_TIMING").map(|path| Trace {
                path: path.into(),
                origin: Instant::now(),
                epoch_us: SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_micros(),
                samples: Mutex::new(VecDeque::new()),
            })
        })
        .as_ref()
}

pub(crate) struct Span(Option<(&'static Trace, &'static str, Instant)>);

pub(crate) fn span(phase: &'static str) -> Span {
    Span(trace().map(|trace| (trace, phase, Instant::now())))
}

impl Drop for Span {
    fn drop(&mut self) {
        if let Some((trace, phase, start)) = self.0 {
            let elapsed = start.elapsed().as_micros();
            if let Ok(mut samples) = trace.samples.lock() {
                if samples.len() == SAMPLE_LIMIT {
                    samples.pop_front();
                }
                samples.push_back((
                    phase,
                    start.duration_since(trace.origin).as_micros(),
                    elapsed,
                ));
            }
        }
    }
}

pub(crate) fn finish() -> std::io::Result<()> {
    use std::io::Write;
    if let Some(trace) = trace() {
        // 显式诊断只在退出后落盘；create_new 防止覆盖用户已有文件。
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&trace.path)?;
        let mut output = std::io::BufWriter::new(file);
        writeln!(output, "phase,start_epoch_us,duration_us")?;
        if let Ok(samples) = trace.samples.lock() {
            for (phase, start, elapsed) in samples.iter() {
                writeln!(output, "{phase},{},{elapsed}", trace.epoch_us + start)?;
            }
        }
        output.flush()?;
    }
    Ok(())
}
