//! Implementations of [`brain::MusicSource`], one per place music comes from.

pub mod local;
pub mod netease;

pub use local::LocalSource;
pub use netease::NeteaseSource;
