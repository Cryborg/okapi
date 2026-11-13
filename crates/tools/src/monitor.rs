//! What the dashboard shows of the system, read off the interface's thread:
//! the mounted volumes and their space, the memory and the applications
//! holding it, the processor, the system. [`Sampler`] runs one thread that
//! reads what is due and sends [`Sample`]s; nothing here waits on a
//! filesystem that stopped answering, nor makes its caller wait.

mod memory;
mod processor;
mod round;
mod sampler;
mod schedule;
mod system;
mod volumes;

pub use memory::{ApplicationMemory, ApplicationName, Applications, MemoryReading};
pub use okapi_platform::{MemoryStatus, Space};
pub use processor::ProcessorReading;
pub use sampler::{Command, Sample, Sampler, Unavailable};
pub use schedule::Source;
pub use system::SystemReading;
pub use volumes::{Volume, VolumeKind, VolumeName, VolumeSpace, nearly_full};
