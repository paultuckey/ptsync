use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use std::sync::OnceLock;

static MULTI_PROGRESS: OnceLock<MultiProgress> = OnceLock::new();

pub fn get_multi_progress() -> &'static MultiProgress {
    MULTI_PROGRESS.get_or_init(MultiProgress::new)
}

#[derive(Clone)]
pub(crate) struct Progress {
    pb: ProgressBar,
}

impl Progress {
    pub(crate) fn new(total: u64) -> Self {
        let mp = get_multi_progress();
        let pb = mp.add(ProgressBar::new(total));
        pb.set_style(
            ProgressStyle::with_template("[{bar:20}] {pos} of {len}")
                .unwrap_or_else(|_| ProgressStyle::default_bar())
                .progress_chars("=> "),
        );
        Progress { pb }
    }

    pub(crate) fn inc(&self) {
        self.pb.inc(1);
    }
}

impl Drop for Progress {
    fn drop(&mut self) {
        self.pb.finish_and_clear();
    }
}

pub struct IndicatifWriter;

impl std::io::Write for IndicatifWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        get_multi_progress().suspend(|| std::io::stderr().write(buf))
    }

    fn flush(&mut self) -> std::io::Result<()> {
        std::io::stderr().flush()
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for IndicatifWriter {
    type Writer = IndicatifWriter;

    fn make_writer(&'a self) -> Self::Writer {
        IndicatifWriter
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use std::time::Duration;
    use tracing::debug;

    /// A demo rather than an assertion. The bar has a frame rate, so raise the
    /// delay to watch it.
    #[test]
    fn test_progress() -> anyhow::Result<()> {
        crate::test_util::setup_log();
        let delay = Duration::from_millis(100);
        let prog = Progress::new(10);
        thread::sleep(delay);
        for i in 0..10 {
            prog.inc();
            if i % 2 == 0 {
                debug!("Even {i}");
            }
            thread::sleep(delay);
        }
        Ok(())
    }
}
