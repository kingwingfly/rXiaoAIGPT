//! Implementations of [`brain::MusicSource`], one per place music comes from.
//!
//! `brain` says only *what* a source must do; everything about *where* the
//! music is lives here. Keeping the implementations behind the trait is what
//! lets the intent layer offer "play something" without knowing whether the
//! answer is a file on this disk or a signed URL from NetEase.

pub mod local;
pub mod netease;

pub use local::LocalSource;
pub use netease::NeteaseSource;
