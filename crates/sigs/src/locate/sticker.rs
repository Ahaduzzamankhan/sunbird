//! The chat pane's sticker send, where `phoenix.messages.onStickerPick` sees
//! a sticker the picker or autocomplete chose.

use crate::{
   Found,
   index::Index,
};

/// `ChatPaneView::chat_send_sticker_now`, `fn(&mut ChatPaneView, sticker,
/// bool, bool, ...)`, which every sticker choice goes through.
///
/// Its async block's span is entered by one function, called only by the
/// entity update that runs it, called only by the send itself.
pub fn sticker_send(ix: &Index<'_>) -> Found<usize> {
   let span = ix
      .literal(b"::chat_send_sticker_now::{{closure}}")
      .one("sticker send span")?;
   let update = span.callers().one("sticker send update")?;
   update.callers().one("sticker send").map(|func| func.start)
}
