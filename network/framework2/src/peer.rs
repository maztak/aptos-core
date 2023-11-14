// Copyright © Aptos Foundation

use std::sync::Arc;
use std::time::Duration;
use tokio::runtime::Handle;
use tokio::sync::mpsc::Receiver;
use crate::protocols::wire::messaging::v1::{ErrorCode, MultiplexMessage, MultiplexMessageSink, MultiplexMessageStream, NetworkMessage};
use futures::io::{AsyncRead,AsyncReadExt,AsyncWrite};
use futures::StreamExt;
use futures::SinkExt;
use futures::stream::Fuse;
use tokio::sync::mpsc::error::TryRecvError;
use aptos_config::config::{NetworkConfig, RoleType};
use aptos_config::network_id::{NetworkContext, NetworkId, PeerNetworkId};
use aptos_logger::{error, info, warn};
use aptos_metrics_core::{IntCounter, IntCounterVec, register_int_counter_vec};
use crate::application::ApplicationCollector;
use crate::application::interface::{OpenRpcRequestState, OutboundRpcMatcher};
use crate::application::storage::PeersAndMetadata;
use crate::ProtocolId;
use crate::protocols::network::{Closer, OutboundPeerConnections, PeerStub, ReceivedMessage};
use crate::protocols::stream::{StreamFragment, StreamHeader, StreamMessage};
use crate::transport::ConnectionMetadata;
use once_cell::sync::Lazy;
use crate::counters;

pub fn start_peer<TSocket>(
    config: &NetworkConfig,
    socket: TSocket,
    connection_metadata: ConnectionMetadata,
    apps: Arc<ApplicationCollector>,
    handle: Handle,
    remote_peer_network_id: PeerNetworkId,
    peers_and_metadata: Arc<PeersAndMetadata>,
    peer_senders: Arc<OutboundPeerConnections>,
    network_context: NetworkContext,
)
where
    TSocket: crate::transport::TSocket
{
    let role_type = network_context.role();
    let (sender, to_send) = tokio::sync::mpsc::channel::<NetworkMessage>(config.network_channel_size);
    let (sender_high_prio, to_send_high_prio) = tokio::sync::mpsc::channel::<NetworkMessage>(config.network_channel_size);
    let open_outbound_rpc = OutboundRpcMatcher::new();
    let max_frame_size = config.max_frame_size;
    let (read_socket, write_socket) = socket.split();
    let reader =
        MultiplexMessageStream::new(read_socket, max_frame_size).fuse();
    let writer = MultiplexMessageSink::new(write_socket, max_frame_size);
    let closed = Closer::new();
    let network_id = remote_peer_network_id.network_id();
    handle.spawn(open_outbound_rpc.clone().cleanup(Duration::from_millis(100), closed.clone()));
    handle.spawn(writer_task(network_id, to_send, to_send_high_prio, writer, max_frame_size, closed.clone()));
    handle.spawn(reader_task(reader, apps, remote_peer_network_id, open_outbound_rpc.clone(), handle.clone(), closed.clone(), role_type));
    let stub = PeerStub::new(sender, sender_high_prio, open_outbound_rpc, closed.clone());
    // TODO: start_peer counter, (PeersAndMetadata keeps gauge, count event here)
    if let Err(err) = peers_and_metadata.insert_connection_metadata(remote_peer_network_id, connection_metadata.clone()) {
        error!("start_peer PeersAndMetadata could not insert metadata: {:?}", err);
    }
    peer_senders.insert(remote_peer_network_id, stub);
    handle.spawn(peer_cleanup_task(remote_peer_network_id, connection_metadata, closed, peers_and_metadata, peer_senders));
}

/// state needed in writer_task()
struct WriterContext<WriteThing: AsyncWrite + Unpin + Send> {
    network_id: NetworkId,
    /// increment for each new fragment stream
    stream_request_id : u32,
    /// remaining payload bytes of curretnly fragmenting large message
    large_message: Option<Vec<u8>>,
    /// index into chain of fragments
    large_fragment_id: u8,
    /// toggle to send normal msg or send fragment of large message
    send_large: bool,
    /// if we have a large message in flight and another arrives, stash it here
    next_large_msg: Option<NetworkMessage>,
    /// TODO: pull this from node config
    max_frame_size: usize,

    /// messages from apps to send to the peer
    to_send: Receiver<NetworkMessage>,
    to_send_high_prio: Receiver<NetworkMessage>,
    /// encoder wrapper around socket write half
    writer: MultiplexMessageSink<WriteThing>,
}

impl<WriteThing: AsyncWrite + Unpin + Send> WriterContext<WriteThing> {
    fn new(
        network_id: NetworkId,
        to_send: Receiver<NetworkMessage>,
        to_send_high_prio: Receiver<NetworkMessage>,
        writer: MultiplexMessageSink<WriteThing>,
        max_frame_size: usize,
    ) -> Self {
        Self {
            network_id,
            stream_request_id: 0,
            large_message: None,
            large_fragment_id: 0,
            send_large: false,
            next_large_msg: None,
            max_frame_size,
            to_send,
            to_send_high_prio,
            writer,
        }
    }

    /// send a next chunk from a currently fragmenting large message
    fn next_large(&mut self) -> MultiplexMessage {
        let mut blob = self.large_message.take().unwrap();
        if blob.len() > self.max_frame_size {
            let rest = blob.split_off(self.max_frame_size);
            self.large_message = Some(rest);
        }
        self.large_fragment_id += 1;
        self.send_large = false;
        MultiplexMessage::Stream(StreamMessage::Fragment(StreamFragment {
            request_id: self.stream_request_id,
            fragment_id: self.large_fragment_id,
            raw_data: blob,
        }))
    }

    fn start_large(&mut self, msg: NetworkMessage) -> MultiplexMessage {
        self.stream_request_id += 1;
        self.send_large = false;
        self.large_fragment_id = 0;
        let mut num_fragments = msg.data_len() / self.max_frame_size;
        let mut msg = msg;
        while num_fragments * self.max_frame_size < msg.data_len() {
            num_fragments += 1;
        }
        if num_fragments > 0x0ff {
            panic!("huge message cannot be fragmented {:?} > 255 * {:?}", msg.data_len(), self.max_frame_size);
        }
        let num_fragments = num_fragments as u8;
        let rest = match &mut msg {
            NetworkMessage::Error(_) => {
                unreachable!("NetworkMessage::Error should always fit in a single frame")
            },
            NetworkMessage::RpcRequest(request) => {
                request.raw_request.split_off(self.max_frame_size)
            },
            NetworkMessage::RpcResponse(response) => {
                response.raw_response.split_off(self.max_frame_size)
            },
            NetworkMessage::DirectSendMsg(message) => {
                message.raw_msg.split_off(self.max_frame_size)
            },
        };
        self.large_message = Some(rest);
        MultiplexMessage::Stream(StreamMessage::Header(StreamHeader {
            request_id: self.stream_request_id,
            num_fragments,
            message: msg,
        }))
    }

    fn try_high_prio_next_msg(&mut self) -> Option<MultiplexMessage> {
        match self.to_send_high_prio.try_recv() {
            Ok(msg) => {
                info!("writer_thread to_send_high_prio {} bytes prot={}", msg.data_len(), msg.protocol_id_as_str());
                if msg.data_len() > self.max_frame_size {
                    // finish prior large message before starting a new large message
                    self.next_large_msg = Some(msg);
                    Some(self.next_large())
                } else {
                    // send small message now, large chunk next
                    self.send_large = true;
                    Some(MultiplexMessage::Message(msg))
                }
            }
            Err(_) => {
                None
            }
        }
    }

    async fn try_next_msg(&mut self) -> Option<MultiplexMessage> {
        if let Some(mm) = self.try_high_prio_next_msg() {
            return Some(mm);
        }
        match self.to_send.try_recv() {
            Ok(msg) => {
                info!("writer_thread to_send {} bytes prot={}", msg.data_len(), msg.protocol_id_as_str());
                if msg.data_len() > self.max_frame_size {
                    // finish prior large message before starting a new large message
                    self.next_large_msg = Some(msg);
                    Some(self.next_large())
                } else {
                    // send small message now, large chunk next
                    self.send_large = true;
                    Some(MultiplexMessage::Message(msg))
                }
            }
            Err(err) => match err {
                TryRecvError::Empty => {
                    // ok, no next small msg, continue with chunks of large message
                    Some(self.next_large())
                }
                TryRecvError::Disconnected => {
                    info!("writer_thread source closed");
                    None
                }
            }
        }
    }

    async fn run(mut self, mut closed: Closer) {
        loop {
            let mm = if self.large_message.is_some() {
                if self.send_large || self.next_large_msg.is_some() {
                    self.next_large()
                } else {
                    match self.try_next_msg().await {
                        None => {break}
                        Some(mm) => {mm}
                    }
                }
            } else if self.next_large_msg.is_some() {
                let msg = self.next_large_msg.take().unwrap();
                self.start_large(msg)
            } else {
                // try high-prio, otherwise wait on whatever is available next
                if let Some(mm) = self.try_high_prio_next_msg() {
                    mm
                } else {
                    tokio::select! {
                        high_prio = self.to_send_high_prio.recv() => match high_prio {
                            None => {
                                info!("writer_thread high prio closed");
                                break;
                            },
                            Some(msg) => {
                                if msg.data_len() > self.max_frame_size {
                                    // start stream
                                    self.start_large(msg)
                                } else {
                                    MultiplexMessage::Message(msg)
                                }
                            }
                        },
                        send_result = self.to_send.recv() => match send_result {
                            None => {
                                info!("writer_thread source closed");
                                break;
                            },
                            Some(msg) => {
                                if msg.data_len() > self.max_frame_size {
                                    // start stream
                                    self.start_large(msg)
                                } else {
                                    MultiplexMessage::Message(msg)
                                }
                            },
                        },
                        // TODO: why does select on close.wait() work below but I did this workaround here?
                        wait_result = closed.done.wait_for(|x| *x) => {
                            info!("writer_thread wait result {:?}", wait_result);
                            break;
                        },
                    }
                }
            };
            if let MultiplexMessage::Message(NetworkMessage::Error(ErrorCode::DisconnectCommand)) = &mm {
                // if let NetworkMessage::Error(ec) = &nm {
                //     match ec {
                //         ErrorCode::DisconnectCommand => {
                            info!("writer_thread got DisconnectCommand");
                            break;
                    //     }
                    //     _ => {}
                    // }
                // }
            }
            let data_len = mm.data_len();
            tokio::select! {
                send_result = self.writer.send(&mm) => match send_result {
                    Ok(_) => {
                        peer_message_frames_written(&self.network_id).inc();
                        peer_message_bytes_written(&self.network_id).inc_by(data_len as u64);
                    }
                    Err(err) => {
                        // TODO: counter net write err
                        warn!("writer_thread error sending message to peer: {:?}", err);
                        break;
                    }
                },
                _ = closed.wait() => {
                    info!("writer_thread peer writer got closed");
                    break;
                }
            }
        }
        closed.close().await;
        info!("writer_thread closing");
    }

    // fn split_message(&self, msg: &mut NetworkMessage) -> Vec<u8> {
    //     match msg {
    //         NetworkMessage::Error(_) => {
    //             unreachable!("NetworkMessage::Error should always fit in a single frame")
    //         },
    //         NetworkMessage::RpcRequest(request) => {
    //             request.raw_request.split_off(self.max_frame_size)
    //         },
    //         NetworkMessage::RpcResponse(response) => {
    //             response.raw_response.split_off(self.max_frame_size)
    //         },
    //         NetworkMessage::DirectSendMsg(message) => {
    //             message.raw_msg.split_off(self.max_frame_size)
    //         },
    //     }
    // }
}

pub static NETWORK_PEER_MESSAGE_FRAMES_WRITTEN: Lazy<IntCounterVec> = Lazy::new(||
    register_int_counter_vec!(
    "aptos_network_frames_written",
    "Number of messages written to MultiplexMessageSink",
    &["network_id"]
).unwrap()
);
pub fn peer_message_frames_written(network_id: &NetworkId) -> IntCounter {
    NETWORK_PEER_MESSAGE_FRAMES_WRITTEN.with_label_values(&[network_id.as_str()])
}

pub static NETWORK_PEER_MESSAGE_BYTES_WRITTEN: Lazy<IntCounterVec> = Lazy::new(||
    register_int_counter_vec!(
    "aptos_network_bytes_written",
    "Number of bytes written to MultiplexMessageSink",
    &["network_id"]
).unwrap()
);
pub fn peer_message_bytes_written(network_id: &NetworkId) -> IntCounter {
    NETWORK_PEER_MESSAGE_BYTES_WRITTEN.with_label_values(&[network_id.as_str()])
}

async fn writer_task(
    network_id: NetworkId,
    to_send: Receiver<NetworkMessage>,
    to_send_high_prio: Receiver<NetworkMessage>,
    writer: MultiplexMessageSink<impl AsyncWrite + Unpin + Send + 'static>,
    max_frame_size: usize,
    closed: Closer,
) {
    let wt = WriterContext::new(network_id, to_send, to_send_high_prio, writer, max_frame_size);
    wt.run(closed).await;
    info!("peer writer exited")
}

async fn complete_rpc(rpc_state: OpenRpcRequestState, nmsg: NetworkMessage) {
    if let NetworkMessage::RpcResponse(response) = nmsg {
        let blob = response.raw_response;
        let now = tokio::time::Instant::now(); // TODO: use a TimeService
        let dt = now.duration_since(rpc_state.started);
        let data_len = blob.len() as u64;
        match rpc_state.sender.send(Ok(blob.into())) {
            Ok(_) => {
                counters::rpc_message_bytes(rpc_state.network_id, rpc_state.protocol_id.as_str(), rpc_state.role_type, counters::RESPONSE_LABEL, counters::INBOUND_LABEL, "delivered", data_len);
                counters::outbound_rpc_request_latency(rpc_state.role_type, rpc_state.network_id, rpc_state.protocol_id).observe(dt.as_secs_f64());
            }
            Err(_) => {
                counters::rpc_message_bytes(rpc_state.network_id, rpc_state.protocol_id.as_str(), rpc_state.role_type, counters::RESPONSE_LABEL, counters::INBOUND_LABEL, "declined", data_len);
            }
        }
    } else {
        unreachable!("read_thread complete_rpc called on other than NetworkMessage::RpcResponse")
    }
}

struct ReaderContext<ReadThing: AsyncRead + Unpin + Send> {
    reader: Fuse<MultiplexMessageStream<ReadThing>>,
    apps: Arc<ApplicationCollector>,
    remote_peer_network_id: PeerNetworkId,
    open_outbound_rpc: OutboundRpcMatcher,
    handle: Handle,
    role_type: RoleType, // for metrics

    // defragment context
    current_stream_id : u32,
    large_message : Option<NetworkMessage>,
    fragment_index : u8,
    num_fragments : u8,
}

impl<ReadThing: AsyncRead + Unpin + Send> ReaderContext<ReadThing> {
    fn new(
        reader: Fuse<MultiplexMessageStream<ReadThing>>,
        apps: Arc<ApplicationCollector>,
        remote_peer_network_id: PeerNetworkId,
        open_outbound_rpc: OutboundRpcMatcher,
        handle: Handle,
        role_type: RoleType,
    ) -> Self {
        Self {
            reader,
            apps,
            remote_peer_network_id,
            open_outbound_rpc,
            handle,
            role_type,

            current_stream_id: 0,
            large_message: None,
            fragment_index: 0,
            num_fragments: 0,
        }
    }

    async fn forward(&self, protocol_id: ProtocolId, nmsg: NetworkMessage) {
        match self.apps.get(&protocol_id) {
            None => {
                // TODO: counter
                error!("read_thread got rpc req for protocol {:?} we don't handle", protocol_id);
                // TODO: drop connection
            }
            Some(app) => {
                if app.protocol_id != protocol_id {
                    for (xpi, ac) in self.apps.iter() {
                        error!("read_thread app err {} -> {} {} {:?}", xpi.as_str(), ac.protocol_id, ac.label, &ac.sender);
                    }
                    unreachable!("read_thread apps[{}] => {} {:?}", protocol_id, app.protocol_id, &app.sender);
                }
                let data_len = nmsg.data_len() as u64;
                match app.sender.try_send(ReceivedMessage{ message: nmsg, sender: self.remote_peer_network_id }) {
                    Ok(_) => {
                        peer_read_message_bytes(&self.remote_peer_network_id.network_id(), &protocol_id, data_len);
                    }
                    Err(_) => {
                        app_inbound_drop(&self.remote_peer_network_id.network_id(), &protocol_id, data_len);
                    }
                }
            }
        }
    }

    async fn handle_message(&self, nmsg: NetworkMessage) {
        match &nmsg {
            NetworkMessage::Error(errm) => {
                // TODO: counter
                warn!("read_thread got error message: {:?}", errm)
            }
            NetworkMessage::RpcRequest(request) => {
                let protocol_id = request.protocol_id;
                let data_len = request.raw_request.len() as u64;
                counters::rpc_message_bytes(self.remote_peer_network_id.network_id(), protocol_id.as_str(), self.role_type, counters::REQUEST_LABEL, counters::INBOUND_LABEL, counters::RECEIVED_LABEL, data_len);
                self.forward(protocol_id, nmsg).await;
            }
            NetworkMessage::RpcResponse(response) => {
                match self.open_outbound_rpc.remove(&response.request_id) {
                    None => {
                        let data_len = response.raw_response.len() as u64;
                        counters::rpc_message_bytes(self.remote_peer_network_id.network_id(), "unk", self.role_type, counters::RESPONSE_LABEL, counters::INBOUND_LABEL, "miss", data_len);
                    }
                    Some(rpc_state) => {
                        self.handle.spawn(complete_rpc(rpc_state, nmsg));//response.raw_response));
                    }
                }
            }
            NetworkMessage::DirectSendMsg(message) => {
                let protocol_id = message.protocol_id;
                let data_len = message.raw_msg.len() as u64;
                counters::direct_send_message_bytes(self.remote_peer_network_id.network_id(), protocol_id.as_str(), self.role_type, counters::RECEIVED_LABEL, data_len);
                self.forward(protocol_id, nmsg).await;
            }
        }
    }

    async fn handle_stream(&mut self, fragment: StreamMessage) {
        match fragment {
            StreamMessage::Header(head) => {
                if self.num_fragments != self.fragment_index {
                    warn!("fragment index = {:?} of {:?} total fragments with new stream header", self.fragment_index, self.num_fragments);
                }
                info!("read_thread shed id={}, {}b {}", head.request_id, head.message.data_len(), head.message.protocol_id_as_str());
                self.current_stream_id = head.request_id;
                self.num_fragments = head.num_fragments;
                self.large_message = Some(head.message);
                self.fragment_index = 1;
            }
            StreamMessage::Fragment(more) => {
                if more.request_id != self.current_stream_id {
                    warn!("got stream request_id={:?} while {:?} was in progress", more.request_id, self.current_stream_id);
                    // TODO: counter? disconnect from peer?
                    self.num_fragments = 0;
                    self.fragment_index = 0;
                    return;
                }
                if more.fragment_id != self.fragment_index {
                    warn!("got fragment_id {:?}, expected {:?}", more.fragment_id, self.fragment_index);
                    // TODO: counter? disconnect from peer?
                    self.num_fragments = 0;
                    self.fragment_index = 0;
                    return;
                }
                info!("read_thread more id={}, {}b", more.request_id, more.raw_data.len());
                match self.large_message.as_mut() {
                    None => {
                        warn!("got fragment without header");
                        return;
                    }
                    Some(lm) => match lm {
                        NetworkMessage::Error(_) => {
                            unreachable!("stream fragment should never be NetworkMessage::Error")
                        }
                        NetworkMessage::RpcRequest(request) => {
                            request.raw_request.extend_from_slice(more.raw_data.as_slice());
                        }
                        NetworkMessage::RpcResponse(response) => {
                            response.raw_response.extend_from_slice(more.raw_data.as_slice());
                        }
                        NetworkMessage::DirectSendMsg(message) => {
                            message.raw_msg.extend_from_slice(more.raw_data.as_slice());
                        }
                    }
                }
                self.fragment_index += 1;
                if self.fragment_index == self.num_fragments {
                    let large_message = self.large_message.take().unwrap();
                    self.handle_message(large_message).await;
                }
            }
        }
    }

    async fn run(mut self, mut closed: Closer) {
        info!("read_thread start");
        loop {
            let rrmm = tokio::select! {
                rrmm = self.reader.next() => {rrmm},
                _ = closed.done.wait_for(|x| *x) => {
                    info!("read_thread {} got closed", self.remote_peer_network_id);
                    return;
                },
            };
            match rrmm {
                // TODO: counter for inbound frames/fragments?
                Some(rmm) => match rmm {
                    Ok(msg) => match msg {
                        MultiplexMessage::Message(nmsg) => {
                            self.handle_message(nmsg).await;
                        }
                        MultiplexMessage::Stream(fragment) => {
                            self.handle_stream(fragment).await;
                        }
                    }
                    Err(err) => {
                        info!("read_thread {} err {}", self.remote_peer_network_id, err);
                    }
                }
                None => {
                    info!("read_thread {} None", self.remote_peer_network_id);
                    break;
                }
            };
        }

        closed.close().await;
    }
}

async fn reader_task(
    reader: Fuse<MultiplexMessageStream<impl AsyncRead + Unpin + Send>>,
    apps: Arc<ApplicationCollector>,
    remote_peer_network_id: PeerNetworkId,
    open_outbound_rpc: OutboundRpcMatcher,
    handle: Handle,
    closed: Closer,
    role_type: RoleType,
) {
    let rc = ReaderContext::new(reader, apps, remote_peer_network_id, open_outbound_rpc, handle, role_type);
    rc.run(closed).await;
    info!("peer {} reader finished", remote_peer_network_id);
}

async fn peer_cleanup_task(
    remote_peer_network_id: PeerNetworkId,
    connection_metadata: ConnectionMetadata,
    mut closed: Closer,
    peers_and_metadata: Arc<PeersAndMetadata>,
    peer_senders: Arc<OutboundPeerConnections>,
) {
    closed.wait().await;
    info!("peer {} closed, cleanup", remote_peer_network_id);
    peer_senders.remove(&remote_peer_network_id);
    _ = peers_and_metadata.remove_peer_metadata(remote_peer_network_id, connection_metadata.connection_id);
}

pub static NETWORK_PEER_READ_MESSAGES: Lazy<IntCounterVec> = Lazy::new(||
    register_int_counter_vec!(
    "aptos_network_peer_read_messages",
    "Number of messages read (after de-frag)",
    &["network_id", "protocol_id"]
).unwrap()
);

pub static NETWORK_PEER_READ_BYTES: Lazy<IntCounterVec> = Lazy::new(||
    register_int_counter_vec!(
    "aptos_network_peer_read_bytes",
    "Number of message bytes read (after de-frag)",
    &["network_id", "protocol_id"]
).unwrap()
);
pub fn peer_read_message_bytes(network_id: &NetworkId, protocol_id: &ProtocolId, data_len: u64) {
    let values = [network_id.as_str(), protocol_id.as_str()];
    NETWORK_PEER_READ_MESSAGES.with_label_values(&values).inc();
    NETWORK_PEER_READ_BYTES.with_label_values(&values).inc_by(data_len);
}

pub static NETWORK_APP_INBOUND_DROP_MESSAGES: Lazy<IntCounterVec> = Lazy::new(||
    register_int_counter_vec!(
    "aptos_network_app_inbound_drop_messages",
    "Number of messages received but dropped before app",
    &["network_id", "protocol_id"]
).unwrap()
);
pub static NETWORK_APP_INBOUND_DROP_BYTES: Lazy<IntCounterVec> = Lazy::new(||
    register_int_counter_vec!(
    "aptos_network_app_inbound_drop_bytes",
    "Number of bytes received but dropped before app",
    &["network_id", "protocol_id"]
).unwrap()
);
pub fn app_inbound_drop(network_id: &NetworkId, protocol_id: &ProtocolId, data_len: u64) {
    let values = [network_id.as_str(), protocol_id.as_str()];
    NETWORK_APP_INBOUND_DROP_MESSAGES.with_label_values(&values).inc();
    NETWORK_APP_INBOUND_DROP_BYTES.with_label_values(&values).inc_by(data_len);
}