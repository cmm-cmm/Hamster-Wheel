//! Stage timings for controlled FastCGI measurements only.
//! The default server build does not compile this module or its recording path.

use std::{sync::Mutex, time::Duration};

const MAX_SAMPLES: usize = 100_000;

#[derive(Clone, Copy, Debug)]
pub struct FastCgiStageTiming {
    pub slot_wait: Duration,
    pub tcp_connect: Duration,
    pub preamble_write: Duration,
    /// From a written request preamble to a complete CGI response head. This
    /// includes endpoint TCP backlog, FastCGI decoding, PHP and scheduling.
    pub backend_head: Duration,
}

#[derive(Debug, Default)]
pub(crate) struct FastCgiProfile {
    samples: Mutex<Vec<FastCgiStageTiming>>,
}

impl FastCgiProfile {
    pub(crate) fn record(&self, sample: FastCgiStageTiming) {
        let mut samples = self
            .samples
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if samples.len() < MAX_SAMPLES {
            samples.push(sample);
        }
    }

    pub(crate) fn take(&self) -> Vec<FastCgiStageTiming> {
        let mut samples = self
            .samples
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        std::mem::take(&mut *samples)
    }
}
