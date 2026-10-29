//! Terminal rendering: the Display surface, progress/spinners, markdown rows,
//! palette, layout, cursor control, commit-type vocabulary, reasoning feed,
//! steps timeline, one-shot command lines.

pub mod commit_type;
pub mod cursor;
pub mod display;
pub mod layout;
pub mod markdown;
pub mod oneshot;
pub mod palette;
pub mod progress;
pub mod reasoning_feed;
pub mod timeline;
pub(crate) mod tui;
