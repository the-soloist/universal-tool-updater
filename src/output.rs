use std::io::{self, Write};
use std::sync::{Mutex, OnceLock};

use indicatif::MultiProgress;
use tracing_subscriber::fmt::MakeWriter;

static ACTIVE_PROGRESS: OnceLock<Mutex<Option<MultiProgress>>> = OnceLock::new();

fn active_progress() -> &'static Mutex<Option<MultiProgress>> {
    ACTIVE_PROGRESS.get_or_init(|| Mutex::new(None))
}

pub fn activate_progress(multi: &MultiProgress) {
    *active_progress()
        .lock()
        .expect("progress output mutex poisoned") = Some(multi.clone());
}

pub fn deactivate_progress() {
    *active_progress()
        .lock()
        .expect("progress output mutex poisoned") = None;
}

pub struct ProgressAwareMakeWriter;

impl<'a> MakeWriter<'a> for ProgressAwareMakeWriter {
    type Writer = ProgressAwareWriter;

    fn make_writer(&'a self) -> Self::Writer {
        ProgressAwareWriter { buffer: Vec::new() }
    }
}

pub struct ProgressAwareWriter {
    buffer: Vec<u8>,
}

impl ProgressAwareWriter {
    fn emit(&mut self) -> io::Result<()> {
        if self.buffer.is_empty() {
            return Ok(());
        }
        let buffer = std::mem::take(&mut self.buffer);
        let progress = active_progress()
            .lock()
            .expect("progress output mutex poisoned")
            .clone();
        if let Some(progress) = progress {
            progress.suspend(|| {
                let mut stderr = io::stderr().lock();
                stderr.write_all(&buffer)?;
                stderr.flush()
            })
        } else {
            let mut stderr = io::stderr().lock();
            stderr.write_all(&buffer)?;
            stderr.flush()
        }
    }
}

impl Write for ProgressAwareWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.buffer.extend_from_slice(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.emit()
    }
}

impl Drop for ProgressAwareWriter {
    fn drop(&mut self) {
        let _ = self.emit();
    }
}
