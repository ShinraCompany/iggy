use std::sync::Arc;
use tracing::error;
use super::message_batch::RetainedMessageBatch;
use crate::streaming::models::messages::RetainedMessage;

pub trait IntoBatchIterator {
    type Item;
    type IntoIter: Iterator<Item = Self::Item>;
    fn into_iter(self) -> Self::IntoIter;
}

pub struct RetainedMessageBatchIterator {
    batch: Arc<RetainedMessageBatch>,
    current_position: u32,
}

impl RetainedMessageBatchIterator {
    pub fn new(batch: Arc<RetainedMessageBatch>) -> Self {
        RetainedMessageBatchIterator {
            batch,
            current_position: 0,
        }
    }
}

// TODO(numinex): Consider using FallibleIterator instead of Option
// https://crates.io/crates/fallible-iterator
impl Iterator for RetainedMessageBatchIterator {
    type Item = RetainedMessage;
    fn next(&mut self) -> Option<Self::Item> {
        //error!("curr_pos: {}, batch_len: {}", self.current_position, self.batch.length);
        if self.current_position < self.batch.length {
            let start_position = self.current_position as usize;
            let length = u32::from_le_bytes(
                self.batch.bytes[start_position..start_position + 4]
                    .try_into()
                    .ok()?,
            );
            let message = self
                .batch
                .bytes
                .slice(start_position + 4..start_position + 4 + length as usize);
            self.current_position += 4 + length;
            RetainedMessage::try_from_bytes(message).ok()
        } else {
            error!("Im done with iterating");
            None
        }
    }
}

impl IntoBatchIterator for Arc<RetainedMessageBatch> {
    type Item = RetainedMessage;
    type IntoIter = RetainedMessageBatchIterator;

    fn into_iter(self) -> Self::IntoIter {
        RetainedMessageBatchIterator::new(self)
    }
}

