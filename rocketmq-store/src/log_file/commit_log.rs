/*
 * Licensed to the Apache Software Foundation (ASF) under one or more
 * contributor license agreements.  See the NOTICE file distributed with
 * this work for additional information regarding copyright ownership.
 * The ASF licenses this file to You under the Apache License, Version 2.0
 * (the "License"); you may not use this file except in compliance with
 * the License.  You may obtain a copy of the License at
 *
 *     http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

use std::{cell::Cell, collections::HashMap, mem, sync::Arc};

use bytes::{Buf, Bytes, BytesMut};
use rocketmq_common::{
    common::{
        attribute::cq_type::CQType,
        broker::broker_config::BrokerConfig,
        config::TopicConfig,
        message::{
            message_single::{tags_string2tags_code, MessageExtBrokerInner},
            MessageConst, MessageVersion,
        },
        mix_all,
        sys_flag::message_sys_flag::MessageSysFlag,
    },
    utils::{queue_type_utils::QueueTypeUtils, time_utils},
    CRC32Utils::crc32,
    MessageDecoder::{
        string_to_message_properties, MESSAGE_MAGIC_CODE_POSITION, MESSAGE_MAGIC_CODE_V2,
        SYSFLAG_POSITION,
    },
    UtilAll::time_millis_to_human_string,
};
use tokio::time::Instant;
use tracing::{error, info, warn};

use crate::{
    base::{
        append_message_callback::DefaultAppendMessageCallback,
        commit_log_dispatcher::CommitLogDispatcher,
        dispatch_request::DispatchRequest,
        message_result::PutMessageResult,
        message_status_enum::{AppendMessageStatus, PutMessageStatus},
        put_message_context::PutMessageContext,
        select_result::SelectMappedBufferResult,
        store_checkpoint::StoreCheckpoint,
        swappable::Swappable,
    },
    config::{broker_role::BrokerRole, message_store_config::MessageStoreConfig},
    consume_queue::mapped_file_queue::MappedFileQueue,
    log_file::mapped_file::{default_impl::DefaultMappedFile, MappedFile},
    message_encoder::message_ext_encoder::MessageExtEncoder,
    message_store::default_message_store::{CommitLogDispatcherDefault, DefaultMessageStore},
    queue::{local_file_consume_queue_store::ConsumeQueueStore, ConsumeQueueStoreTrait},
};

// Message's MAGIC CODE daa320a7
pub const MESSAGE_MAGIC_CODE: i32 = -626843481;

// End of file empty MAGIC CODE cbd43194
pub const BLANK_MAGIC_CODE: i32 = -875286124;

//CRC32 Format: [PROPERTY_CRC32 + NAME_VALUE_SEPARATOR + 10-digit fixed-length string +
// PROPERTY_SEPARATOR]
pub const CRC32_RESERVED_LEN: i32 = (MessageConst::PROPERTY_CRC32.len() + 1 + 10 + 1) as i32;

struct PutMessageThreadLocal {
    encoder: Cell<Option<MessageExtEncoder>>,
    key: Cell<String>,
}

thread_local! {
    static PUT_MESSAGE_THREAD_LOCAL: PutMessageThreadLocal = PutMessageThreadLocal{
        encoder: Cell::new(None),
        key: Cell::new(String::new()),
    };
}

fn encode_message_ext(
    message_ext: &MessageExtBrokerInner,
    message_store_config: &Arc<MessageStoreConfig>,
) -> (Option<PutMessageResult>, BytesMut) {
    PUT_MESSAGE_THREAD_LOCAL.with(|thread_local| match thread_local.encoder.take() {
        None => {
            let mut encoder = MessageExtEncoder::new(Arc::clone(message_store_config));
            let result = encoder.encode(message_ext);
            let bytes_mut = encoder.byte_buf();
            thread_local.encoder.set(Some(encoder));
            (result, bytes_mut)
        }

        Some(mut encoder) => {
            let result = encoder.encode(message_ext);
            let bytes_mut = encoder.byte_buf();
            thread_local.encoder.set(Some(encoder));
            (result, bytes_mut)
        }
    })
}

fn generate_key(msg: &MessageExtBrokerInner) -> String {
    PUT_MESSAGE_THREAD_LOCAL.with(|thead_local| {
        let mut topic_queue_key = thead_local.key.take();
        topic_queue_key.clear();
        topic_queue_key.push_str(msg.topic());
        topic_queue_key.push('-');
        topic_queue_key.push_str(msg.queue_id().to_string().as_str());
        let key_return = topic_queue_key.to_string();
        thead_local.key.set(topic_queue_key);
        key_return
    })
}

pub fn get_cq_type(
    topic_config_table: &Arc<parking_lot::Mutex<HashMap<String, TopicConfig>>>,
    msg_inner: &MessageExtBrokerInner,
) -> CQType {
    let option = topic_config_table.lock().get(msg_inner.topic()).cloned();
    QueueTypeUtils::get_cq_type(&option)
}

pub fn get_message_num(
    topic_config_table: &Arc<parking_lot::Mutex<HashMap<String, TopicConfig>>>,
    msg_inner: &MessageExtBrokerInner,
) -> i16 {
    let mut message_num = 1i16;
    let cq_type = get_cq_type(topic_config_table, msg_inner);
    if MessageSysFlag::check(msg_inner.sys_flag(), MessageSysFlag::INNER_BATCH_FLAG)
        || cq_type == CQType::BatchCQ
    {
        if let Some(num) = msg_inner
            .message_ext_inner
            .message
            .get_property(MessageConst::PROPERTY_INNER_NUM)
        {
            message_num = num.parse().unwrap_or(1i16);
        }
    }
    // message_num
    message_num
}

#[derive(Clone)]
pub struct CommitLog {
    mapped_file_queue: MappedFileQueue,
    message_store_config: Arc<MessageStoreConfig>,
    broker_config: Arc<BrokerConfig>,
    enabled_append_prop_crc: bool,
    //local_file_message_store: Option<Weak<Mutex<LocalFileMessageStore>>>,
    dispatcher: CommitLogDispatcherDefault,
    confirm_offset: i64,
    store_checkpoint: Arc<StoreCheckpoint>,
    append_message_callback: Arc<DefaultAppendMessageCallback>,
    put_message_lock: Arc<tokio::sync::Mutex<()>>,
    topic_config_table: Arc<parking_lot::Mutex<HashMap<String, TopicConfig>>>,
    consume_queue_store: ConsumeQueueStore,
}

impl CommitLog {
    pub fn new(
        message_store_config: Arc<MessageStoreConfig>,
        broker_config: Arc<BrokerConfig>,
        dispatcher: &CommitLogDispatcherDefault,
        store_checkpoint: Arc<StoreCheckpoint>,
        topic_config_table: Arc<parking_lot::Mutex<HashMap<String, TopicConfig>>>,
        consume_queue_store: ConsumeQueueStore,
    ) -> Self {
        let enabled_append_prop_crc = message_store_config.enabled_append_prop_crc;
        let store_path = message_store_config.get_store_path_commit_log();
        let mapped_file_size = message_store_config.mapped_file_size_commit_log;
        Self {
            mapped_file_queue: MappedFileQueue::new(store_path, mapped_file_size as u64, None),
            message_store_config: message_store_config.clone(),
            broker_config,
            enabled_append_prop_crc,
            //local_file_message_store: None,
            dispatcher: dispatcher.clone(),
            confirm_offset: -1,
            store_checkpoint,
            append_message_callback: Arc::new(DefaultAppendMessageCallback::new(
                message_store_config,
                topic_config_table.clone(),
            )),
            put_message_lock: Arc::new(Default::default()),
            topic_config_table,
            consume_queue_store,
        }
    }
}

#[allow(unused_variables)]
impl CommitLog {
    pub fn load(&mut self) -> bool {
        let result = self.mapped_file_queue.load();
        self.mapped_file_queue.check_self();
        info!("load commit log {}", if result { "OK" } else { "Failed" });
        result
    }

    pub fn shutdown(&mut self) {}

    pub fn destroy(&mut self) {}
    /*    pub fn set_local_file_message_store(
        &mut self,
        local_file_message_store: Weak<Mutex<LocalFileMessageStore>>,
    ) {
       // self.local_file_message_store = Some(local_file_message_store);
    }*/

    pub fn set_confirm_offset(&mut self, phy_offset: i64) {
        self.confirm_offset = phy_offset;
        self.store_checkpoint
            .set_confirm_phy_offset(phy_offset as u64);
    }

    pub async fn put_message(&mut self, msg: MessageExtBrokerInner) -> PutMessageResult {
        let mut msg = msg;
        if !self.message_store_config.duplication_enable {
            msg.message_ext_inner.store_timestamp = time_utils::get_current_millis() as i64;
        }
        msg.message_ext_inner.body_crc = crc32(
            msg.message_ext_inner
                .message
                .body
                .as_ref()
                .unwrap()
                .as_ref(),
        );
        if !self.enabled_append_prop_crc {
            msg.delete_property(MessageConst::PROPERTY_CRC32);
        }

        //setting message version
        msg.with_version(MessageVersion::V1);
        let topic = msg.topic();
        // setting auto message on topic length
        if self.message_store_config.auto_message_version_on_topic_len
            && topic.len() > i8::MAX as usize
        {
            msg.with_version(MessageVersion::V2);
        }

        //setting ip type:IPV4 OR IPV6, default is ipv4
        let born_host = msg.born_host();
        if born_host.is_ipv6() {
            msg.with_born_host_v6_flag();
        }

        let store_host = msg.store_host();
        if store_host.is_ipv6() {
            msg.with_store_host_v6_flag();
        }

        let topic_queue_key = generate_key(&msg);

        let mut _unlock_mapped_file = None;
        let mut mapped_file = self.mapped_file_queue.get_last_mapped_file();
        let curr_offset = if let Some(ref mapped_file_inner) = mapped_file {
            mapped_file_inner.get_wrote_position() as u64 + mapped_file_inner.get_file_from_offset()
        } else {
            0
        };
        let need_ack_nums = self.message_store_config.in_sync_replicas;
        let need_handle_ha = self.need_handle_ha(&msg);
        if need_handle_ha && self.broker_config.enable_controller_mode {
            unimplemented!("controller mode not support HA")
        } else if need_handle_ha && self.broker_config.enable_slave_acting_master {
            unimplemented!("slave acting master not support HA")
        }

        let need_assign_offset = !(self.message_store_config.duplication_enable
            && self.message_store_config.broker_role != BrokerRole::Slave);

        if need_assign_offset {
            self.assign_offset(&mut msg);
        }

        let (put_message_result, encoded_buff) =
            encode_message_ext(&msg, &self.message_store_config);
        if let Some(result) = put_message_result {
            return result;
        }
        msg.encoded_buff = Some(encoded_buff);
        let put_message_context = PutMessageContext::new(topic_queue_key);
        let lock = self.put_message_lock.lock().await;
        let start_time = Instant::now();
        // Here settings are stored timestamp, in order to ensure an orderly global
        if !self.message_store_config.duplication_enable {
            msg.message_ext_inner.store_timestamp = time_utils::get_current_millis() as i64;
        }

        if mapped_file.is_none() || mapped_file.as_ref().unwrap().is_full() {
            mapped_file = self
                .mapped_file_queue
                .get_last_mapped_file_mut_start_offset(0, true);
        }

        if mapped_file.is_none() {
            drop(lock);
            return PutMessageResult::new_default(PutMessageStatus::CreateMappedFileFailed);
        }

        let result = mapped_file.as_ref().unwrap().append_message(
            &mut msg,
            self.append_message_callback.as_ref(),
            &put_message_context,
        );
        let put_message_result = match result.status {
            AppendMessageStatus::PutOk => {
                //onCommitLogAppend(msg, result, mappedFile); in java not support this version
                PutMessageResult::new_append_result(PutMessageStatus::PutOk, Some(result))
            }
            AppendMessageStatus::EndOfFile => {
                //onCommitLogAppend(msg, result, mappedFile); in java not support this version
                _unlock_mapped_file = mapped_file;
                mapped_file = self
                    .mapped_file_queue
                    .get_last_mapped_file_mut_start_offset(0, true);
                if mapped_file.is_none() {
                    error!(
                        "create mapped file error, topic: {}  clientAddr: {}",
                        msg.topic(),
                        msg.born_host()
                    );
                    return PutMessageResult::new_append_result(
                        PutMessageStatus::CreateMappedFileFailed,
                        Some(result),
                    );
                }
                let result = mapped_file.as_ref().unwrap().append_message(
                    &mut msg,
                    self.append_message_callback.as_ref(),
                    &put_message_context,
                );
                if AppendMessageStatus::PutOk == result.status {
                    PutMessageResult::new_append_result(PutMessageStatus::PutOk, Some(result))
                } else {
                    PutMessageResult::new_append_result(
                        PutMessageStatus::UnknownError,
                        Some(result),
                    )
                }
            }
            AppendMessageStatus::MessageSizeExceeded
            | AppendMessageStatus::PropertiesSizeExceeded => {
                PutMessageResult::new_append_result(PutMessageStatus::MessageIllegal, Some(result))
            }
            AppendMessageStatus::UnknownError => {
                PutMessageResult::new_append_result(PutMessageStatus::UnknownError, Some(result))
            }
        };
        let elapsed_time_in_lock = start_time.elapsed().as_millis() as u64;
        drop(lock);
        if elapsed_time_in_lock > 100 {
            warn!(
                "[NOTIFYME]putMessage in lock cost time(ms)={}, bodyLength={} \
                 AppendMessageResult={:?}",
                elapsed_time_in_lock,
                msg.body_len(),
                put_message_result.append_message_result().as_ref().unwrap(),
            );
        }

        if put_message_result.put_message_status() == PutMessageStatus::PutOk {
            let message_num = get_message_num(&self.topic_config_table, &msg);
            self.increase_offset(&msg, message_num);
            self.handle_disk_flush_and_ha(put_message_result, msg, need_ack_nums, need_handle_ha)
                .await
        } else {
            put_message_result
        }
    }

    fn increase_offset(&self, msg: &MessageExtBrokerInner, message_num: i16) {
        let tran_type = MessageSysFlag::get_transaction_value(msg.sys_flag());
        if MessageSysFlag::TRANSACTION_NOT_TYPE == tran_type
            || MessageSysFlag::TRANSACTION_COMMIT_TYPE == tran_type
        {
            self.consume_queue_store
                .increase_queue_offset(msg, message_num);
        }
    }

    fn assign_offset(&self, msg: &mut MessageExtBrokerInner) {
        let tran_type = MessageSysFlag::get_transaction_value(msg.sys_flag());
        if MessageSysFlag::TRANSACTION_NOT_TYPE == tran_type
            || MessageSysFlag::TRANSACTION_COMMIT_TYPE == tran_type
        {
            self.consume_queue_store.assign_queue_offset(msg);
        }
    }

    async fn handle_disk_flush_and_ha(
        &self,
        put_message_result: PutMessageResult,
        msg: MessageExtBrokerInner,
        need_ack_nums: u32,
        need_handle_ha: bool,
    ) -> PutMessageResult {
        put_message_result
    }

    fn need_handle_ha(&self, msg_inner: &MessageExtBrokerInner) -> bool {
        if !msg_inner.is_wait_store_msg_ok() {
            /*
             No need to sync messages that special config to extra broker slaves.
             @see MessageConst.PROPERTY_WAIT_STORE_MSG_OK
            */
            return false;
        }
        if self.message_store_config.duplication_enable {
            return false;
        }
        if BrokerRole::SyncMaster != self.message_store_config.broker_role {
            // No need to check ha in async or slave broker
            return false;
        }

        true
    }

    fn on_commit_log_dispatch(
        &mut self,
        request: &DispatchRequest,
        do_dispatch: bool,
        is_recover: bool,
        is_file_end: bool,
    ) {
        if do_dispatch && !is_file_end {
            self.dispatcher.dispatch(request);
        }
    }

    pub fn is_multi_dispatch_msg(msg_inner: &MessageExtBrokerInner) -> bool {
        msg_inner
            .property(MessageConst::PROPERTY_INNER_MULTI_DISPATCH)
            .map_or(false, |s| !s.is_empty())
            && msg_inner
                .topic()
                .starts_with(mix_all::RETRY_GROUP_TOPIC_PREFIX)
    }

    pub async fn recover_normally(
        &mut self,
        max_phy_offset_of_consume_queue: i64,
        mut message_store: DefaultMessageStore,
    ) {
        let check_crc_on_recover = self.message_store_config.check_crc_on_recover;
        let check_dup_info = self.message_store_config.duplication_enable;
        let message_store_config = self.message_store_config.clone();
        let broker_config = self.broker_config.clone();
        // let mut mapped_file_queue = mapped_files.write().await;
        let mapped_files = self.mapped_file_queue.get_mapped_files();
        let mapped_files_inner = mapped_files.read();
        if !mapped_files_inner.is_empty() {
            // Began to recover from the last third file
            let mut index = (mapped_files_inner.len() as i32) - 3;
            if index <= 0 {
                index = 0;
            }
            let mut index = index as usize;
            //let mut mapped_file = mapped_files_inner.get(index).unwrap().lock().await;
            let mut mapped_file = mapped_files_inner.get(index).unwrap();
            let mut process_offset = mapped_file.get_file_from_offset();
            let mut mapped_file_offset = 0u64;
            //When recovering, the maximum value obtained when getting get_confirm_offset is
            // the file size of the latest file plus the value resolved from the file name.
            let mut last_valid_msg_phy_offset = self.get_confirm_offset() as u64;
            // normal recover doesn't require dispatching
            let do_dispatch = false;
            let mut current_pos = 0usize;
            loop {
                let (msg, size) = self.get_simple_message_bytes(current_pos, mapped_file.as_ref());
                if msg.is_none() {
                    break;
                }
                let mut msg_bytes = msg.unwrap();
                let dispatch_request = check_message_and_return_size(
                    &mut msg_bytes,
                    check_crc_on_recover,
                    check_dup_info,
                    true,
                    &message_store_config,
                );
                current_pos += size;
                if dispatch_request.success && dispatch_request.msg_size > 0 {
                    last_valid_msg_phy_offset = process_offset + mapped_file_offset;
                    mapped_file_offset += dispatch_request.msg_size as u64;
                    self.on_commit_log_dispatch(&dispatch_request, do_dispatch, true, false);
                } else if dispatch_request.success && dispatch_request.msg_size == 0 {
                    // Come the end of the file, switch to the next file Since the
                    // return 0 representatives met last hole,
                    // this can not be included in truncate offset
                    self.on_commit_log_dispatch(&dispatch_request, do_dispatch, true, true);
                    index += 1;
                    if index >= mapped_files_inner.len() {
                        info!(
                            "recover last 3 physics file over, last mapped file:{} ",
                            mapped_file.get_file_name()
                        );
                        break;
                    } else {
                        mapped_file = mapped_files_inner.get(index).unwrap();
                        mapped_file_offset = 0;
                        process_offset = mapped_file.get_file_from_offset();
                        current_pos = 0;
                        info!("recover next physics file:{}", mapped_file.get_file_name());
                    }
                } else if !dispatch_request.success {
                    if dispatch_request.msg_size > 0 {
                        warn!(
                            "found a half message at {}, it will be truncated.",
                            process_offset + mapped_file_offset,
                        );
                    }
                    info!("recover physics file end,{} ", mapped_file.get_file_name());
                    break;
                }
            }
            process_offset += mapped_file_offset;
            if broker_config.enable_controller_mode {
                unimplemented!();
            } else {
                self.set_confirm_offset(last_valid_msg_phy_offset as i64);
            }

            // Clear ConsumeQueue redundant data
            if max_phy_offset_of_consume_queue as u64 >= process_offset {
                warn!(
                    "maxPhyOffsetOfConsumeQueue({}) >= processOffset({}), truncate dirty logic \
                     files",
                    max_phy_offset_of_consume_queue, process_offset
                );
                message_store.truncate_dirty_logic_files(process_offset as i64)
            }
            self.mapped_file_queue
                .set_flushed_where(process_offset as i64);
            self.mapped_file_queue
                .set_committed_where(process_offset as i64);
            self.mapped_file_queue
                .truncate_dirty_files(process_offset as i64);
        } else {
            warn!(
                "The commitlog files are deleted, and delete the consume queue
                             files"
            );
            self.mapped_file_queue.set_flushed_where(0);
            self.mapped_file_queue.set_committed_where(0);
            message_store.consume_queue_store_mut().destroy();
            message_store.consume_queue_store_mut().load_after_destroy();
        }
    }

    fn get_simple_message_bytes<MF: MappedFile>(
        &self,
        position: usize,
        mapped_file: &MF,
    ) -> (Option<Bytes>, usize) {
        let mut bytes = mapped_file.get_bytes(position, 4);
        match bytes {
            None => (None, 0),
            Some(ref mut inner) => {
                let size = inner.get_i32();
                if size <= 0 {
                    return (None, 0);
                }
                (
                    mapped_file.get_bytes(position, size as usize),
                    size as usize,
                )
            }
        }
    }

    //Fetch and compute the newest confirmOffset.
    pub fn get_confirm_offset(&self) -> i64 {
        if self.broker_config.enable_controller_mode {
            unimplemented!()
        } else if self.broker_config.duplication_enable {
            return self.confirm_offset;
        }
        self.get_max_offset()
    }

    pub async fn recover_abnormally(
        &mut self,
        max_phy_offset_of_consume_queue: i64,
        mut message_store: DefaultMessageStore,
    ) {
        let check_crc_on_recover = self.message_store_config.check_crc_on_recover;
        let check_dup_info = self.message_store_config.duplication_enable;
        //let message_store_config = self.message_store_config.clone();
        let broker_config = self.broker_config.clone();
        // let mut mapped_file_queue = mapped_files.write().await;
        let binding = self.mapped_file_queue.get_mapped_files();
        let mapped_files_inner = binding.read();
        if !mapped_files_inner.is_empty() {
            // Began to recover from the last third file
            let mut index = (mapped_files_inner.len() as i32) - 1;
            while index >= 0 {
                let mapped_file = mapped_files_inner.get(index as usize).unwrap();
                if is_mapped_file_matched_recover(
                    &self.message_store_config,
                    mapped_file,
                    &self.store_checkpoint,
                ) {
                    break;
                }
                index -= 1;
            }
            if index <= 0 {
                index = 0;
            }
            let mut index = index as usize;
            //let mut mapped_file = mapped_files_inner.get(index).unwrap().lock().await;
            let mut mapped_file = mapped_files_inner.get(index).unwrap();
            let mut process_offset = mapped_file.get_file_from_offset();
            let mut mapped_file_offset = 0u64;
            //When recovering, the maximum value obtained when getting get_confirm_offset is
            // the file size of the latest file plus the value resolved from the file name.
            let mut last_valid_msg_phy_offset = process_offset;
            let mut last_confirm_valid_msg_phy_offset = process_offset;
            // normal recover doesn't require dispatching
            let do_dispatch = true;
            let mut current_pos = 0usize;
            loop {
                let (msg, size) = self.get_simple_message_bytes(current_pos, mapped_file.as_ref());
                if msg.is_none() {
                    break;
                }
                let mut msg_bytes = msg.unwrap();
                let dispatch_request = check_message_and_return_size(
                    &mut msg_bytes,
                    check_crc_on_recover,
                    check_dup_info,
                    true,
                    &self.message_store_config,
                );
                current_pos += size;
                if dispatch_request.success && dispatch_request.msg_size > 0 {
                    last_valid_msg_phy_offset = process_offset + mapped_file_offset;
                    mapped_file_offset += dispatch_request.msg_size as u64;

                    if self.message_store_config.duplication_enable
                        || self.broker_config.enable_controller_mode
                    {
                        if dispatch_request.commit_log_offset + size as i64
                            <= self.get_confirm_offset()
                        {
                            self.on_commit_log_dispatch(
                                &dispatch_request,
                                do_dispatch,
                                true,
                                false,
                            );
                            last_confirm_valid_msg_phy_offset =
                                dispatch_request.commit_log_offset as u64 + size as u64;
                        }
                    } else {
                        self.on_commit_log_dispatch(&dispatch_request, do_dispatch, true, false);
                    }
                } else if dispatch_request.success && dispatch_request.msg_size == 0 {
                    // Come the end of the file, switch to the next file Since the
                    // return 0 representatives met last hole,
                    // this can not be included in truncate offset
                    self.on_commit_log_dispatch(&dispatch_request, do_dispatch, true, true);
                    index += 1;
                    if index >= mapped_files_inner.len() {
                        info!(
                            "recover last 3 physics file over, last mapped file:{} ",
                            mapped_file.get_file_name()
                        );
                        break;
                    } else {
                        mapped_file = mapped_files_inner.get(index).unwrap();
                        mapped_file_offset = 0;
                        process_offset = mapped_file.get_file_from_offset();
                        current_pos = 0;
                        info!("recover next physics file:{}", mapped_file.get_file_name());
                    }
                } else if !dispatch_request.success {
                    if dispatch_request.msg_size > 0 {
                        warn!(
                            "found a half message at {}, it will be truncated.",
                            process_offset + mapped_file_offset,
                        );
                    }
                    info!("recover physics file end,{} ", mapped_file.get_file_name());
                    break;
                }
            }

            // only for rocksdb mode
            // this.getMessageStore().finishCommitLogDispatch();

            process_offset += mapped_file_offset;
            if broker_config.enable_controller_mode {
                println!(
                    "TODO: finishCommitLogDispatch:{}",
                    last_confirm_valid_msg_phy_offset
                );
                unimplemented!();
            } else {
                self.set_confirm_offset(last_valid_msg_phy_offset as i64);
            }

            // Clear ConsumeQueue redundant data
            if max_phy_offset_of_consume_queue as u64 >= process_offset {
                warn!(
                    "maxPhyOffsetOfConsumeQueue({}) >= processOffset({}), truncate dirty logic \
                     files",
                    max_phy_offset_of_consume_queue, process_offset
                );
                message_store.truncate_dirty_logic_files(process_offset as i64)
            }
            self.mapped_file_queue
                .set_flushed_where(process_offset as i64);
            self.mapped_file_queue
                .set_committed_where(process_offset as i64);
            self.mapped_file_queue
                .truncate_dirty_files(process_offset as i64);
        } else {
            warn!(
                "The commitlog files are deleted, and delete the consume queue
                             files"
            );
            self.mapped_file_queue.set_flushed_where(0);
            self.mapped_file_queue.set_committed_where(0);
            message_store.consume_queue_store_mut().destroy();
            message_store.consume_queue_store_mut().load_after_destroy();
        }
    }

    pub fn get_max_offset(&self) -> i64 {
        self.mapped_file_queue.get_max_offset()
    }

    pub fn get_min_offset(&self) -> i64 {
        match self.mapped_file_queue.get_first_mapped_file() {
            None => -1,
            Some(mapped_file) => {
                if mapped_file.is_available() {
                    mapped_file.get_file_from_offset() as i64
                } else {
                    self.roll_next_file(mapped_file.get_file_from_offset() as i64)
                }
            }
        }
    }

    pub fn roll_next_file(&self, offset: i64) -> i64 {
        let mapped_file_size = self.message_store_config.mapped_file_size_commit_log as i64;
        offset + mapped_file_size - (offset % mapped_file_size)
    }

    pub fn get_data(&self, offset: i64) -> Option<SelectMappedBufferResult> {
        self.get_data_with_option(offset, offset == 0)
    }
    pub fn get_data_with_option(
        &self,
        offset: i64,
        return_first_on_not_found: bool,
    ) -> Option<SelectMappedBufferResult> {
        let mapped_file_size = self.message_store_config.mapped_file_size_commit_log as i64;
        let mapped_file = self
            .mapped_file_queue
            .find_mapped_file_by_offset(offset, return_first_on_not_found);
        if let Some(mapped_file) = mapped_file {
            let pos = (offset % mapped_file_size) as i32;
            DefaultMappedFile::select_mapped_buffer(mapped_file, pos)
        } else {
            None
        }
    }

    pub fn check_self(&self) {
        self.mapped_file_queue.check_self();
    }
}

pub fn check_message_and_return_size(
    bytes: &mut Bytes,
    check_crc: bool,
    check_dup_info: bool,
    read_body: bool,
    message_store_config: &Arc<MessageStoreConfig>,
) -> DispatchRequest {
    let total_size = bytes.get_i32();
    let magic_code = bytes.get_i32();
    if magic_code == MESSAGE_MAGIC_CODE || magic_code == MESSAGE_MAGIC_CODE_V2 {
    } else if magic_code == BLANK_MAGIC_CODE {
        return DispatchRequest {
            msg_size: 0,
            success: true,
            ..Default::default()
        };
    } else {
        warn!(
            "found a illegal magic code 0x{}",
            format!("{:X}", magic_code),
        );
        return DispatchRequest {
            msg_size: -1,
            success: false,
            ..Default::default()
        };
    }
    let message_version = MessageVersion::value_of_magic_code(magic_code).unwrap();
    let body_crc = bytes.get_i32();
    let queue_id = bytes.get_i32();
    let flag = bytes.get_i32();
    let queue_offset = bytes.get_i64();
    let physic_offset = bytes.get_i64();
    let sys_flag = bytes.get_i32();
    let born_time_stamp = bytes.get_i64();

    let born_host = if sys_flag & MessageSysFlag::BORNHOST_V6_FLAG == 0 {
        bytes.copy_to_bytes(8)
    } else {
        bytes.copy_to_bytes(20)
    };
    let store_timestamp = bytes.get_i64();

    let store_host = if sys_flag & MessageSysFlag::STOREHOSTADDRESS_V6_FLAG == 0 {
        bytes.copy_to_bytes(8)
    } else {
        bytes.copy_to_bytes(20)
    };

    let reconsume_times = bytes.get_i32();
    let prepared_transaction_offset = bytes.get_i64();
    let body_len = bytes.get_i32();
    if body_len > 0 {
        if read_body {
            let body = bytes.copy_to_bytes(body_len as usize);
            if check_crc && !message_store_config.force_verify_prop_crc {
                let crc = crc32(body.as_ref());
                if crc != body_crc as u32 {
                    warn!("CRC check failed. bodyCRC={}, currentCRC={}", crc, body_crc);
                    return DispatchRequest {
                        msg_size: -1,
                        success: false,
                        ..Default::default()
                    };
                }
            }
        } else {
            bytes.advance(body_len as usize);
        }
    }
    let topic_len = message_version.get_topic_length(bytes);
    let topic_bytes = bytes.copy_to_bytes(topic_len);
    let topic = String::from_utf8_lossy(topic_bytes.as_ref()).to_string();
    let properties_length = bytes.get_i16();
    let (tags_code, keys, uniq_key, properties_map) = if properties_length > 0 {
        let properties = bytes.copy_to_bytes(properties_length as usize);
        let properties_content = String::from_utf8_lossy(topic_bytes.as_ref()).to_string();
        let properties_map = string_to_message_properties(Some(&properties_content));
        let keys = properties_map.get(MessageConst::PROPERTY_KEYS).cloned();
        let uniq_key = properties_map
            .get(MessageConst::PROPERTY_UNIQ_CLIENT_MESSAGE_ID_KEYIDX)
            .cloned();
        if check_dup_info {
            let dup_info = properties_map.get(MessageConst::DUP_INFO).cloned();
            if dup_info.is_none() {
                warn!("DupInfo in properties check failed. dupInfo=null");
                return DispatchRequest {
                    msg_size: -1,
                    success: false,
                    ..Default::default()
                };
            } else {
                let content = dup_info.unwrap();
                let vec = content.split('_').collect::<Vec<&str>>();
                if vec.len() != 2 {
                    warn!("DupInfo in properties check failed. dupInfo={}", content);
                    return DispatchRequest {
                        msg_size: -1,
                        success: false,
                        ..Default::default()
                    };
                }
            }
        }
        {
            // Timing message processing
        }
        let tags = properties_map.get(MessageConst::PROPERTY_TAGS);
        let tags_code = tags_string2tags_code(tags);
        (
            tags_code,
            keys.unwrap_or("".to_string()),
            uniq_key,
            properties_map,
        )
    } else {
        (0, "".to_string(), None, HashMap::new())
    };

    if check_crc && !message_store_config.force_verify_prop_crc {
        let _expected_crc = -1i32;
        if !properties_map.is_empty() {}
    }

    let read_length = MessageExtEncoder::cal_msg_length(
        message_version,
        sys_flag,
        body_len,
        topic_len as i32,
        properties_length as i32,
    );

    if total_size != read_length {
        error!(
            "[BUG]read total count not equals msg total size. totalSize={}, readTotalCount={}, \
             bodyLen={}, topicLen={}, propertiesLength={}",
            total_size, read_length, body_len, topic_len, properties_length
        );
        return DispatchRequest {
            msg_size: total_size,
            success: false,
            ..Default::default()
        };
    }
    let mut dispatch_request = DispatchRequest {
        success: true,
        topic,
        queue_id,
        commit_log_offset: physic_offset,
        msg_size: total_size,
        tags_code,
        store_timestamp,
        consume_queue_offset: queue_offset,
        keys,
        uniq_key,
        sys_flag,
        prepared_transaction_offset,
        ..DispatchRequest::default()
    };
    set_batch_size_if_needed(&properties_map, &mut dispatch_request);
    dispatch_request.properties_map = Some(properties_map);
    dispatch_request
}

fn set_batch_size_if_needed(
    properties_map: &HashMap<String, String>,
    dispatch_request: &mut DispatchRequest,
) {
    if !properties_map.is_empty()
        && properties_map.contains_key(MessageConst::PROPERTY_INNER_NUM)
        && properties_map.contains_key(MessageConst::PROPERTY_INNER_BASE)
    {
        dispatch_request.msg_base_offset = properties_map
            .get(MessageConst::PROPERTY_INNER_BASE)
            .unwrap()
            .parse::<i64>()
            .unwrap();
        dispatch_request.batch_size = properties_map
            .get(MessageConst::PROPERTY_INNER_NUM)
            .unwrap()
            .parse::<i16>()
            .unwrap();
    }
}

fn is_mapped_file_matched_recover(
    message_store_config: &Arc<MessageStoreConfig>,
    mapped_file: &DefaultMappedFile,
    store_checkpoint: &StoreCheckpoint,
) -> bool {
    let magic_code = mapped_file
        .get_bytes(MESSAGE_MAGIC_CODE_POSITION, mem::size_of::<i32>())
        .unwrap_or(Bytes::from([0u8; mem::size_of::<i32>()].as_ref()))
        .get_i32();

    //check magic code
    if magic_code != MESSAGE_MAGIC_CODE && magic_code != MESSAGE_MAGIC_CODE_V2 {
        return false;
    }
    if message_store_config.is_enable_rocksdb_store() {
        unimplemented!()
    } else {
        let sys_flag = mapped_file
            .get_bytes(SYSFLAG_POSITION, mem::size_of::<i32>())
            .unwrap_or(Bytes::from([0u8; mem::size_of::<i32>()].as_ref()))
            .get_i32();
        let born_host_length = if sys_flag & MessageSysFlag::BORNHOST_V6_FLAG == 0 {
            8
        } else {
            20
        };
        let msg_store_time_pos = 4 + 4 + 4 + 4 + 4 + 8 + 8 + 4 + 8 + born_host_length;
        let store_timestamp = mapped_file
            .get_bytes(msg_store_time_pos, mem::size_of::<i64>())
            .unwrap_or(Bytes::from([0u8; mem::size_of::<i64>()].as_ref()))
            .get_i64();
        if store_timestamp == 0 {
            return false;
        }
        if message_store_config.message_index_enable && message_store_config.message_index_safe {
            if store_timestamp <= store_checkpoint.get_min_timestamp_index() as i64 {
                info!(
                    "find check timestamp, {} {}",
                    store_timestamp,
                    time_millis_to_human_string(store_timestamp)
                );
                return true;
            }
        } else if store_timestamp <= store_checkpoint.get_min_timestamp() as i64 {
            info!(
                "find check timestamp, {} {}",
                store_timestamp,
                time_millis_to_human_string(store_timestamp)
            );
            return true;
        }
    }
    false
}

impl Swappable for CommitLog {
    fn swap_map(
        &self,
        _reserve_num: i32,
        _force_swap_interval_ms: i64,
        _normal_swap_interval_ms: i64,
    ) {
        todo!()
    }

    fn clean_swapped_map(&self, _force_clean_swap_interval_ms: i64) {
        todo!()
    }
}
