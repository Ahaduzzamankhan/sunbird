//! The chat bar's button row, where `phoenix.ui.addComposerButton` draws.

use crate::{
   Found,
   index::Index,
};

/// The chat bar's button row render, the one function loading the gift
/// button wrapper's `gift-button.wrap` element id. On macOS it takes `x0` to
/// `x4` and returns its `AnyElement` through `x8`.
pub fn composer_buttons(ix: &Index<'_>) -> Found<usize> {
   ix.literal(b"gift-button.wrap")
      .one("composer buttons")
      .map(|func| func.start)
}
