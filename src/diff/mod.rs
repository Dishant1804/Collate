//! A diff view for reviewing changes: files of hunks, unified or split, with word changes,
//! syntax colours, expandable unchanged ranges and line selection, on a
//! virtual list that keeps a fifty-thousand-line diff at frame rate.
//!
//! [`parse`] reads a patch (`git diff`) and [`FileDiff::from_texts`] diffs two texts. The app
//! keeps a [`DiffState`] of the files in its state and draws it with [`DiffView`].
//!
//! Adapted from rune-kit's `diff` (MIT, see `LICENSE`) and restyled for Collate.

mod find;
mod model;
pub(crate) mod myers;
mod state;
mod view;

pub use find::{Finder, Query};
pub use model::{FileDiff, FileStatus, ImageSide, LineKind, Side, mark_moved, parse};
pub use state::{Anchor, BlameNote, DiffMode, DiffState, IMAGE_H, LineRef, Style};
pub use myers::{WordEdit, apply_edits, word_edits};
pub use view::{DiffView, EDGE, GHOST_PAD};
#[cfg(test)]
pub use {model::Line, state::LineRange};
#[cfg(test)]
pub(crate) use state::Row;
