use okapi_platform::CpuTimes;

/// The processor's use since the last reading, and the load average.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ProcessorReading {
    /// Between 0 and 1; `None` at the first reading (after a pause too), or
    /// when no time passed.
    pub usage: Option<f64>,
    /// `None` when it could not be read.
    pub load_average: Option<f64>,
}

/// The last times read, to measure the use between two readings.
#[derive(Debug, Default)]
pub(crate) struct Processor {
    last: Option<CpuTimes>,
}

impl Processor {
    /// The use since the last reading, which `times` becomes.
    pub(crate) fn usage(&mut self, times: CpuTimes) -> Option<f64> {
        let last = self.last.replace(times)?;
        let total = times.total.saturating_sub(last.total);
        if total == 0 {
            return None;
        }
        let busy = times.busy.saturating_sub(last.busy);
        #[expect(
            clippy::cast_precision_loss,
            reason = "ticks between two readings are far below 2^52"
        )]
        let usage = busy as f64 / total as f64;
        Some(usage.clamp(0.0, 1.0))
    }

    /// Forgets the last reading (after a pause, the use over the whole pause
    /// would be meaningless).
    pub(crate) fn reset(&mut self) {
        self.last = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn times(busy: u64, total: u64) -> CpuTimes {
        CpuTimes { busy, total }
    }

    #[test]
    fn the_use_is_the_share_of_busy_time_between_two_readings() {
        let mut processor = Processor::default();
        assert_eq!(processor.usage(times(100, 1_000)), None);
        assert_eq!(processor.usage(times(130, 1_120)), Some(0.25));
        assert_eq!(processor.usage(times(130, 1_120)), None, "no time passed");
        assert_eq!(
            processor.usage(times(120, 1_220)),
            Some(0.0),
            "a counter that went back"
        );
        assert_eq!(
            processor.usage(times(400, 1_320)),
            Some(1.0),
            "busy beyond the total"
        );
        processor.reset();
        assert_eq!(processor.usage(times(500, 2_000)), None);
    }
}
