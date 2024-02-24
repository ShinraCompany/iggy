use crate::streaming::batching::batch_filter::BatchFilter;
use crate::streaming::batching::message_batch::RetainedMessageBatch;
use crate::streaming::models::messages::RetainedMessage;
use crate::streaming::partitions::partition::Partition;
use crate::streaming::polling_consumer::PollingConsumer;
use crate::streaming::segments::segment::Segment;
use crate::streaming::segments::time_index::TimeIndex;
use bytes::BytesMut;
use iggy::error::IggyError;
use iggy::messages::send_messages::Message;
use iggy::models::messages::POLLED_MESSAGE_METADATA;
use iggy::utils::timestamp::IggyTimestamp;
use std::sync::{atomic::Ordering, Arc};
use tracing::{error, trace, warn};

const EMPTY_MESSAGES: Vec<RetainedMessage> = vec![];
const EMPTY_BATCHES: Vec<RetainedMessageBatch> = vec![];

impl Partition {
    pub fn get_messages_count(&self) -> u64 {
        self.messages_count.load(Ordering::SeqCst)
    }

    pub async fn get_messages_by_timestamp(
        &self,
        timestamp: u64,
        count: u32,
    ) -> Result<Vec<RetainedMessage>, IggyError> {
        trace!(
            "Getting messages by timestamp: {} for partition: {}...",
            timestamp,
            self.partition_id
        );
        if self.segments.is_empty() {
            return Ok(EMPTY_MESSAGES);
        }

        let result = self
            .segments
            .iter()
            .rev()
            .find_map(|segment| {
                segment.time_indexes.as_ref().and_then(|time_indexes| {
                    if time_indexes.is_empty() {
                        return None;
                    }

                    time_indexes
                        .iter()
                        .rposition(|time_index| time_index.timestamp <= timestamp)
                        .map(|idx| {
                            let found_index = time_indexes[idx];
                            let start_offset =
                                segment.start_offset + found_index.relative_offset as u64;
                            trace!(
                                "Found start offset: {} for timestamp: {}.",
                                start_offset,
                                timestamp
                            );
                            if found_index.timestamp == timestamp {
                                return self.get_messages_by_offset(start_offset, count);
                            }

                            let adjusted_count = self.calculate_adjusted_timestamp_message_count(
                                count,
                                timestamp,
                                found_index.timestamp,
                            );
                            self.get_messages_by_offset(start_offset, adjusted_count)
                        })
                })
            })
            .or_else(|| {
                let overfetch_value = self
                    .segments
                    .first()?
                    .time_indexes
                    .as_ref()
                    .map(|time_indexes| time_indexes.first().unwrap().relative_offset);

                let first_time_index = TimeIndex::default();
                let start_offset = first_time_index.relative_offset as u64;

                overfetch_value.map(|overfetch_value| {
                    self.get_messages_by_offset(start_offset, count + overfetch_value)
                })
            });

        match result {
            Some(result) => Ok(result
                .await?
                .into_iter()
                .filter(|msg| msg.timestamp >= timestamp)
                .take(count as usize)
                .collect()),
            None => Ok(EMPTY_MESSAGES),
        }
    }
    fn calculate_adjusted_timestamp_message_count(
        &self,
        count: u32,
        timestamp: u64,
        timestamp_from_index: u64,
    ) -> u32 {
        if self.avg_timestamp_delta == 0 {
            return count;
        }
        let timestamp_diff = timestamp - timestamp_from_index;
        // This approximation is not exact, but it's good enough for the usage of this function
        let overfetch_value =
            ((timestamp_diff as f64 / self.avg_timestamp_delta as f64) * 1.35).ceil() as u32;
        count + overfetch_value
    }

    pub async fn get_messages_by_offset(
        &self,
        start_offset: u64,
        count: u32,
    ) -> Result<Vec<RetainedMessage>, IggyError> {
        trace!(
            "Getting messages for start offset: {} for partition: {}...",
            start_offset,
            self.partition_id
        );
        if self.segments.is_empty() {
            return Ok(EMPTY_MESSAGES);
        }

        if start_offset > self.current_offset {
            return Ok(EMPTY_MESSAGES);
        }

        let end_offset = self.get_end_offset(start_offset, count);

        let messages = self.try_get_messages_from_cache(start_offset, end_offset);
        if let Some(messages) = messages {
            return Ok(messages);
        }

        let segments = self.filter_segments_by_offsets(start_offset, end_offset);
        match segments.len() {
            0 => Ok(EMPTY_MESSAGES),
            1 => segments[0].get_messages(start_offset, count).await,
            _ => Self::get_messages_from_segments(segments, start_offset, count).await,
        }
    }

    pub async fn get_first_messages(&self, count: u32) -> Result<Vec<RetainedMessage>, IggyError> {
        self.get_messages_by_offset(0, count).await
    }

    pub async fn get_last_messages(&self, count: u32) -> Result<Vec<RetainedMessage>, IggyError> {
        let mut count = count as u64;
        if count > self.current_offset + 1 {
            count = self.current_offset + 1
        }

        let start_offset = 1 + self.current_offset - count;
        self.get_messages_by_offset(start_offset, count as u32)
            .await
    }

    pub async fn get_next_messages(
        &self,
        consumer: PollingConsumer,
        count: u32,
    ) -> Result<Vec<RetainedMessage>, IggyError> {
        let (consumer_offsets, consumer_id) = match consumer {
            PollingConsumer::Consumer(consumer_id, _) => (&self.consumer_offsets, consumer_id),
            PollingConsumer::ConsumerGroup(consumer_group_id, _) => {
                (&self.consumer_group_offsets, consumer_group_id)
            }
        };

        let consumer_offset = consumer_offsets.get(&consumer_id);
        if consumer_offset.is_none() {
            trace!(
                "Consumer: {} hasn't stored offset for partition: {}, returning the first messages...",
                consumer_id,
                self.partition_id
            );
            return self.get_first_messages(count).await;
        }

        let consumer_offset = consumer_offset.unwrap();
        if consumer_offset.offset == self.current_offset {
            trace!(
                "Consumer: {} has the latest offset: {} for partition: {}, returning empty messages...",
                consumer_id,
                consumer_offset.offset,
                self.partition_id
            );
            return Ok(EMPTY_MESSAGES);
        }

        let offset = consumer_offset.offset + 1;
        trace!(
            "Getting next messages for {} for partition: {} from offset: {}...",
            consumer_id,
            self.partition_id,
            offset
        );

        self.get_messages_by_offset(offset, count).await
    }

    fn get_end_offset(&self, offset: u64, count: u32) -> u64 {
        let mut end_offset = offset + (count - 1) as u64;
        let segment = self.segments.last().unwrap();
        let max_offset = segment.current_offset;
        if end_offset > max_offset {
            end_offset = max_offset;
        }

        end_offset
    }

    fn filter_segments_by_offsets(&self, start_offset: u64, end_offset: u64) -> Vec<&Segment> {
        let slice_start = self
            .segments
            .iter()
            .rposition(|segment| segment.start_offset <= start_offset)
            .unwrap_or(0);

        self.segments[slice_start..]
            .iter()
            .filter(|segment| segment.start_offset <= end_offset)
            .collect()
    }

    async fn get_messages_from_segments(
        segments: Vec<&Segment>,
        offset: u64,
        count: u32,
    ) -> Result<Vec<RetainedMessage>, IggyError> {
        let mut messages = Vec::with_capacity(segments.len());
        for segment in segments {
            let segment_messages = segment.get_messages(offset, count).await?;
            for message in segment_messages {
                messages.push(message);
            }
        }

        Ok(messages)
    }

    fn try_get_messages_from_cache(
        &self,
        start_offset: u64,
        end_offset: u64,
    ) -> Option<Vec<RetainedMessage>> {
        let cache = self.cache.as_ref()?;
        if cache.is_empty() || start_offset > end_offset || end_offset > self.current_offset {
            return None;
        }

        let first_buffered_offset = cache[0].base_offset;
        trace!(
            "First buffered offset: {} for partition: {}",
            first_buffered_offset,
            self.partition_id
        );

        if start_offset >= first_buffered_offset {
            return Some(self.load_messages_from_cache(start_offset, end_offset));
        }
        None
    }

    pub async fn get_newest_messages_by_size(
        &self,
        size_bytes: u64,
    ) -> Result<Vec<Arc<RetainedMessageBatch>>, IggyError> {
        trace!(
            "Getting messages for size: {} bytes for partition: {}...",
            size_bytes,
            self.partition_id
        );

        if self.segments.is_empty() {
            return Ok(EMPTY_BATCHES.into_iter().map(Arc::new).collect());
        }

        let mut remaining_size = size_bytes;
        let mut batches = Vec::new();
        for segment in self.segments.iter().rev() {
            let segment_size_bytes = segment.size_bytes as u64;
            if segment_size_bytes == 0 {
                break;
            }
            if segment_size_bytes > remaining_size {
                // Last segment is bigger than the remaining size, so we need to get the newest messages from it.
                let partial_messages = segment
                    .get_newest_message_batches_by_size(remaining_size)
                    .await?;
                batches.splice(..0, partial_messages);
                break;
            }

            // Current segment is smaller than the remaining size, so we need to get all messages from it.
            let segment_batches = segment.get_all_batches().await?;
            batches.splice(..0, segment_batches);
            remaining_size = remaining_size.saturating_sub(segment_size_bytes);
            if remaining_size == 0 {
                break;
            }
        }
        error!("batches len: {}", batches.len());

        Ok(batches)
    }

    fn load_messages_from_cache(&self, start_offset: u64, end_offset: u64) -> Vec<RetainedMessage> {
        trace!(
            "Loading messages from cache, start offset: {}, end offset: {}...",
            start_offset,
            end_offset
        );

        if self.cache.is_none() || start_offset > end_offset {
            return EMPTY_MESSAGES;
        }

        let cache = self.cache.as_ref().unwrap();
        if cache.is_empty() {
            return EMPTY_MESSAGES;
        }

        let mut slice_start = 0;
        for idx in (0..cache.len()).rev() {
            if cache[idx].base_offset <= start_offset {
                slice_start = idx;
                break;
            }
        }
        let messages = cache
            .iter()
            .skip(slice_start)
            .filter(|batch| {
                batch.is_contained_or_overlapping_within_offset_range(start_offset, end_offset)
            })
            .convert_and_filter_by_offset_range(start_offset, end_offset);

        let expected_messages_count = (end_offset - start_offset + 1) as usize;
        if messages.len() != expected_messages_count {
            warn!(
                "Loaded {} messages from cache, expected {}.",
                messages.len(),
                expected_messages_count
            );
            return EMPTY_MESSAGES;
        }
        trace!(
            "Loaded {} messages from cache, start offset: {}, end offset: {}...",
            messages.len(),
            start_offset,
            end_offset
        );

        messages
    }

    pub async fn append_messages(&mut self, messages: &Vec<Message>) -> Result<(), IggyError> {
        {
            let last_segment = self.segments.last_mut().ok_or(IggyError::SegmentNotFound)?;
            if last_segment.is_closed {
                let start_offset = last_segment.end_offset + 1;
                trace!(
                    "Current segment is closed, creating new segment with start offset: {} for partition with ID: {}...",
                    start_offset, self.partition_id
                );
                self.add_persisted_segment(start_offset).await?;
            }
        }

        //TODO(numinex): Pass this from the system level + POLLED_MESSAGE_METADATA * messages_count
        let batch_size = messages
            .iter()
            .map(|msg| (msg.get_size_bytes() + POLLED_MESSAGE_METADATA) as usize)
            .sum();

        let base_offset = if !self.should_increment_offset {
            0
        } else {
            self.current_offset + 1
        };

        let mut messages_count = 0u32;
        // assume that messages have monotonic timestamps
        let mut max_timestamp = 0;
        let mut min_timestamp = 0;

        let mut buffer = BytesMut::with_capacity(batch_size);
        let mut batch_builder = RetainedMessageBatch::builder();
        batch_builder = batch_builder.base_offset(base_offset);
        if let Some(message_deduplicator) = &self.message_deduplicator {
            for message in messages {
                if !message_deduplicator.try_insert(&message.id).await {
                    warn!(
                        "Ignored the duplicated message ID: {} for partition with ID: {}.",
                        message.id, self.partition_id
                    );
                    continue;
                }
                max_timestamp = IggyTimestamp::now().to_micros();

                if messages_count == 0 {
                    min_timestamp = max_timestamp;
                }
                let message_offset = base_offset + messages_count as u64;
                let message = RetainedMessage::new(message_offset, max_timestamp, message);
                message.extend(&mut buffer);
                messages_count += 1;
            }
        } else {
            for message in messages {
                max_timestamp = IggyTimestamp::now().to_micros();

                if messages_count == 0 {
                    min_timestamp = max_timestamp;
                }
                let message_offset = base_offset + messages_count as u64;
                let message = RetainedMessage::new(message_offset, max_timestamp, message);
                message.extend(&mut buffer);
                messages_count += 1;
            }
        }
        let avg_timestamp_delta = ((max_timestamp - min_timestamp) / messages_count as u64) as u32;

        let min_alpha: f64 = 0.3;
        let max_alpha: f64 = 0.7;
        let dynamic_range = 10.00;
        self.update_avg_timestamp_delta(avg_timestamp_delta, min_alpha, max_alpha, dynamic_range);

        let last_offset = base_offset + (messages_count - 1) as u64;
        let last_offset_delta = messages_count - 1;
        let batch = Arc::new(
            batch_builder
                .max_timestamp(max_timestamp)
                .last_offset_delta(last_offset_delta)
                .length(buffer.len() as u32)
                .payload(buffer.freeze())
                .build()?,
        );

        if self.should_increment_offset {
            self.current_offset = last_offset;
        } else {
            self.should_increment_offset = true;
            self.current_offset = last_offset;
        }

        {
            let last_segment = self.segments.last_mut().ok_or(IggyError::SegmentNotFound)?;
            last_segment.append_messages(batch.clone()).await?;
        }

        if let Some(cache) = &mut self.cache {
            cache.append(batch);
        }

        self.unsaved_messages_count += messages_count;
        {
            let last_segment = self.segments.last_mut().ok_or(IggyError::SegmentNotFound)?;
            if self.unsaved_messages_count >= self.config.partition.messages_required_to_save
                || last_segment.is_full().await
            {
                trace!(
                    "Segment with start offset: {} for partition with ID: {} will be persisted on disk...",
                    last_segment.start_offset,
                    self.partition_id
                );

                last_segment.persist_messages().await.unwrap();
                self.unsaved_messages_count = 0;
            }
        }

        Ok(())
    }

    fn update_avg_timestamp_delta(
        &mut self,
        avg_timestamp_delta: u32,
        min_alpha: f64,
        max_alpha: f64,
        dynamic_range: f64,
    ) {
        let diff = self.avg_timestamp_delta.abs_diff(avg_timestamp_delta);
        let alpha = max_alpha.min(min_alpha.max(1.0 - (diff as f64 / dynamic_range)));
        self.avg_timestamp_delta = (alpha * avg_timestamp_delta as f64
            + (1.0 - alpha) * self.avg_timestamp_delta as f64)
            as u32;
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicU64;

    use super::*;
    use crate::configs::system::{MessageDeduplicationConfig, SystemConfig};
    use crate::streaming::partitions::create_messages;
    use crate::streaming::storage::tests::get_test_system_storage;

    #[tokio::test]
    async fn given_disabled_message_deduplication_all_messages_should_be_appended() {
        let mut partition = create_partition(false);
        let messages = create_messages();
        let messages_count = messages.len() as u32;
        partition.append_messages(&messages).await.unwrap();

        let loaded_messages = partition
            .get_messages_by_offset(0, messages_count)
            .await
            .unwrap();
        assert_eq!(loaded_messages.len(), messages_count as usize);
    }

    #[tokio::test]
    async fn given_enabled_message_deduplication_only_messages_with_unique_id_should_be_appended() {
        let mut partition = create_partition(true);
        let messages = create_messages();
        let messages_count = messages.len() as u32;
        let unique_messages_count = 3;
        partition.append_messages(&messages).await.unwrap();

        let loaded_messages = partition
            .get_messages_by_offset(0, messages_count)
            .await
            .unwrap();
        assert_eq!(loaded_messages.len(), unique_messages_count);
    }

    fn create_partition(deduplication_enabled: bool) -> Partition {
        let storage = Arc::new(get_test_system_storage());
        let stream_id = 1;
        let topic_id = 2;
        let partition_id = 3;
        let with_segment = true;
        let config = Arc::new(SystemConfig {
            message_deduplication: MessageDeduplicationConfig {
                enabled: deduplication_enabled,
                ..Default::default()
            },
            ..Default::default()
        });
        Partition::create(
            stream_id,
            topic_id,
            partition_id,
            with_segment,
            config,
            storage,
            None,
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicU64::new(0)),
        )
    }
}
