use crate::streaming::cache::memory_tracker::CacheMemoryTracker;
use crate::streaming::models::messages::PolledMessages;
use crate::streaming::polling_consumer::PollingConsumer;
use crate::streaming::session::Session;
use crate::streaming::systems::system::System;
use bytes::Bytes;
use iggy::messages::poll_messages::PollingStrategy;
use iggy::messages::send_messages;
use iggy::messages::send_messages::Partitioning;
use iggy::models::messages::PolledMessage;
use iggy::{error::IggyError, identifier::Identifier};
use std::sync::Arc;
use tracing::{error, trace};

impl System {
    pub async fn poll_messages(
        &self,
        session: &Session,
        consumer: PollingConsumer,
        stream_id: &Identifier,
        topic_id: &Identifier,
        args: PollingArgs,
    ) -> Result<PolledMessages, IggyError> {
        self.ensure_authenticated(session)?;
        if args.count == 0 {
            return Err(IggyError::InvalidMessagesCount);
        }

        let stream = self.get_stream(stream_id)?;
        let topic = stream.get_topic(topic_id)?;
        self.permissioner
            .poll_messages(session.get_user_id(), stream.stream_id, topic.topic_id)?;

        if !topic.has_partitions() {
            return Err(IggyError::NoPartitions(topic.topic_id, topic.stream_id));
        }

        let partition_id = match consumer {
            PollingConsumer::Consumer(_, partition_id) => partition_id,
            PollingConsumer::ConsumerGroup(consumer_group_id, member_id) => {
                let consumer_group = topic
                    .get_consumer_group_by_id(consumer_group_id)?
                    .read()
                    .await;
                consumer_group.calculate_partition_id(member_id).await?
            }
        };

        let mut polled_messages = topic
            .get_messages(consumer, partition_id, args.strategy, args.count)
            .await?;

        if polled_messages.messages.is_empty() {
            return Ok(polled_messages);
        }

        let offset = polled_messages.messages.last().unwrap().offset;
        if args.auto_commit {
            trace!("Last offset: {} will be automatically stored for {}, stream: {}, topic: {}, partition: {}", offset, consumer, stream_id, topic_id, partition_id);
            topic.store_consumer_offset(consumer, offset).await?;
        }

        if self.encryptor.is_none() {
            return Ok(polled_messages);
        }

        let encryptor = self.encryptor.as_ref().unwrap();
        let mut decrypted_messages = Vec::with_capacity(polled_messages.messages.len());
        for message in polled_messages.messages.iter() {
            let payload = encryptor.decrypt(&message.payload);
            match payload {
                Ok(payload) => {
                    decrypted_messages.push(Arc::new(PolledMessage {
                        id: message.id,
                        state: message.state,
                        offset: message.offset,
                        timestamp: message.timestamp,
                        checksum: message.checksum,
                        length: payload.len() as u32,
                        payload: Bytes::from(payload),
                        headers: message.headers.clone(),
                    }));
                }
                Err(error) => {
                    error!("Cannot decrypt the message. Error: {}", error);
                    return Err(IggyError::CannotDecryptData);
                }
            }
        }
        polled_messages.messages = decrypted_messages;
        Ok(polled_messages)
    }

    pub async fn append_messages(
        &self,
        session: &Session,
        stream_id: &Identifier,
        topic_id: &Identifier,
        partitioning: &Partitioning,
        messages: &Vec<send_messages::Message>,
    ) -> Result<(), IggyError> {
        self.ensure_authenticated(session)?;
        let stream = self.get_stream(stream_id)?;
        let topic = stream.get_topic(topic_id)?;
        self.permissioner.append_messages(
            session.get_user_id(),
            stream.stream_id,
            topic.topic_id,
        )?;

        let mut batch_size_bytes = 0;
        if let Some(encryptor) = &self.encryptor {
            let mut encrypted_messages = Vec::with_capacity(messages.len());
            for message in messages.iter() {
                let payload = encryptor.encrypt(&message.payload);
                match payload {
                    Ok(payload) => {
                        let msg = send_messages::Message::new(
                            Some(message.id),
                            Bytes::from(payload),
                            message.headers.clone(),
                        );
                        batch_size_bytes += msg.get_size_bytes() as u64;
                        encrypted_messages.push(msg);
                    }
                    Err(error) => {
                        error!("Cannot encrypt the message. Error: {}", error);
                        return Err(IggyError::CannotEncryptData);
                    }
                }
            }
            if let Some(memory_tracker) = CacheMemoryTracker::get_instance() {
                if !memory_tracker.will_fit_into_cache(batch_size_bytes) {
                    self.clean_cache(batch_size_bytes).await;
                }
            }

            topic
                .append_messages(partitioning, &encrypted_messages)
                .await?;
            self.metrics
                .increment_messages(encrypted_messages.len() as u64);
            Ok(())
        } else {
            batch_size_bytes = messages.iter().map(|msg| msg.get_size_bytes() as u64).sum();
            if let Some(memory_tracker) = CacheMemoryTracker::get_instance() {
                if !memory_tracker.will_fit_into_cache(batch_size_bytes) {
                    self.clean_cache(batch_size_bytes).await;
                }
            }

            topic.append_messages(partitioning, messages).await?;
            self.metrics.increment_messages(messages.len() as u64);
            Ok(())
        }
    }
}

#[derive(Debug)]
pub struct PollingArgs {
    pub strategy: PollingStrategy,
    pub count: u32,
    pub auto_commit: bool,
}

impl PollingArgs {
    pub fn new(strategy: PollingStrategy, count: u32, auto_commit: bool) -> Self {
        Self {
            strategy,
            count,
            auto_commit,
        }
    }
}
