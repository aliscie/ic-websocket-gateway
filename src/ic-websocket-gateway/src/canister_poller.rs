use crate::{
    canister_methods::{
        self, CanisterOpenMessageContent, CanisterOutputCertifiedMessages, CanisterOutputMessage,
        CanisterServiceMessage, CanisterToClientMessage, CanisterWsGetMessagesArguments,
        ClientPrincipal, WebsocketMessage,
    },
    events_analyzer::{Events, EventsCollectionType, EventsReference},
    messages_demux::MessagesDemux,
    metrics::canister_poller_metrics::{PollerEvents, PollerEventsMetrics},
};
use candid::decode_one;
use ic_agent::{export::Principal, Agent};
use serde_cbor::from_slice;
use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio::{
    join, select,
    sync::mpsc::{Receiver, Sender},
};
use tracing::{debug, error, info, trace, warn};

type CanisterGetMessagesWithEvents = (CanisterOutputCertifiedMessages, PollerEvents);

/// ends of the channels needed by each canister poller tasks
#[derive(Debug)]
pub struct PollerChannelsPollerEnds {
    /// receiving side of the channel used by the main task to send the receiving task of a new client's channel to the poller task
    main_to_poller: Receiver<PollerToClientChannelData>,
    /// sending side of the channel used by the poller to send the canister id of the poller which is about to terminate
    poller_to_main: Sender<TerminationInfo>,
    /// sending side of the channel used by the poller to send events to the event analyzer
    pub poller_to_analyzer: Sender<Box<dyn Events + Send>>,
}

impl PollerChannelsPollerEnds {
    pub fn new(
        main_to_poller: Receiver<PollerToClientChannelData>,
        poller_to_main: Sender<TerminationInfo>,
        poller_to_analyzer: Sender<Box<dyn Events + Send>>,
    ) -> Self {
        Self {
            main_to_poller,
            poller_to_main,
            poller_to_analyzer,
        }
    }
}

/// updates the client connection handler on the IC WS connection state
pub enum IcWsConnectionUpdate {
    /// contains a new message to be realyed to the client
    Message(CanisterToClientMessage),
    /// lets the client connection hanlder know that an error occurred and the connection should be closed
    Error(String),
}

/// contains the information that the main task sends to the poller task:
#[derive(Debug, Clone)]
pub enum PollerToClientChannelData {
    /// contains the sending side of the channel use by the poller to send messages to the client
    NewClientChannel(ClientPrincipal, Sender<IcWsConnectionUpdate>),
    /// signals the poller which cllient disconnected
    ClientDisconnected(ClientPrincipal),
}

/// determines the reason of the poller task termination:
pub enum TerminationInfo {
    /// contains the principal of the last client and therefore there is no need to continue polling
    LastClientDisconnected(Principal),
    /// error while polling the canister
    CdkError(Principal),
}

/// periodically polls the canister for updates to be relayed to clients
pub struct CanisterPoller {
    canister_id: Principal,
    agent: Arc<Agent>,
    polling_interval_ms: u64,
}

impl CanisterPoller {
    pub fn new(canister_id: Principal, agent: Arc<Agent>, polling_interval_ms: u64) -> Self {
        Self {
            canister_id,
            agent,
            polling_interval_ms,
        }
    }

    #[tracing::instrument(
        name = "poll_canister",
        skip_all,
        fields(
            canister_id = %self.canister_id
        )
    )]
    pub async fn run_polling(
        &self,
        mut poller_channels: PollerChannelsPollerEnds,
        first_client_principal: ClientPrincipal,
        message_for_client_tx: Sender<IcWsConnectionUpdate>,
    ) {
        // once the poller starts running, it requests messages from nonce 0.
        // if the canister already has some messages in the queue and receives the nonce 0, it knows that the poller restarted
        // therefore, it sends the last X messages to the gateway. From these, the gateway has to determine the response corresponding to the client's ws_open request
        let mut message_nonce = 0;

        // channels used to communicate with the connection handler task of the client identified by the principal
        let mut client_channels: HashMap<ClientPrincipal, Sender<IcWsConnectionUpdate>> =
            HashMap::new();
        // the channel used to send updates to the first client is passed as an argument to the poller
        // this way we can be sure that once the poller gets the first messages from the canister, there is already a client to send them to
        // this also ensures that we can detect which messages in the first polling iteration are "old" and which ones are not
        // this is necessary as the poller once it starts it does not know the nonce of the last message delivered by the canister
        client_channels.insert(first_client_principal, message_for_client_tx);

        // queues where the poller temporarily stores messages received from the canister before a client is registered
        // this is needed because the poller might get a message for a client which is not yet regiatered in the poller
        let mut clients_message_queues: HashMap<ClientPrincipal, Vec<CanisterToClientMessage>> =
            HashMap::new();

        let mut polling_iteration = 0; // used as a reference for the PollerEvents

        let messages_demux = MessagesDemux::new();

        let get_messages_operation =
            self.get_canister_updates(message_nonce, polling_iteration, first_client_principal);
        // pin the tracking of the in-flight asynchronous operation so that in each select! iteration get_messages_operation is continued
        // instead of issuing a new call to get_canister_updates
        tokio::pin!(get_messages_operation);

        'poller_loop: loop {
            select! {
                // receive channel used to send canister updates to new client's task
                Some(channel_data) = poller_channels.main_to_poller.recv() => {
                    match channel_data {
                        PollerToClientChannelData::NewClientChannel(client_principal, client_channel) => {
                            debug!("Added new channel to poller for client: {:?}", client_principal);
                            client_channels.insert(client_principal.clone(), client_channel);
                        },
                        PollerToClientChannelData::ClientDisconnected(client_principal) => {
                            debug!("Removed client channel from poller for client {:?}", client_principal);
                            client_channels.remove(&client_principal);
                            debug!("Removed message queue from poller for client {:?}", client_principal);
                            clients_message_queues.remove(&client_principal);
                            debug!("{} clients connected to poller", client_channels.len());
                            // exit task if last client disconnected
                            if client_channels.is_empty() {
                                info!("Terminating poller task as no clients are connected");
                                signal_poller_task_termination(&mut poller_channels.poller_to_main, TerminationInfo::LastClientDisconnected(self.canister_id)).await;
                                break;
                            }
                        }
                    }
                }
                // poll canister for updates across multiple select! iterations
                res = &mut get_messages_operation => {
                    // process messages in queues before the ones just polled from the canister (if any) so that the clients receive messages in the expected order
                    // this is done even if no messages are returned from the current polling iteration as there might be messages in the queue waiting to be processed
                    process_queues(&mut clients_message_queues, &client_channels).await;

                    if let Some((msgs, mut poller_events)) = res {
                        poller_events.metrics.set_start_relaying_messages();
                        poller_channels
                            .poller_to_analyzer
                            .send(Box::new(poller_events))
                            .await
                            .expect("analyzer's side of the channel dropped");

                        if let Err(e) = messages_demux.relay_messages(
                            msgs,
                            &mut clients_message_queues,
                            &client_channels,
                            &mut poller_channels,
                            &mut message_nonce,
                        ).await {
                            error!(e);
                            signal_termination_and_cleanup(
                                &mut poller_channels.poller_to_main,
                                self.canister_id,
                                &client_channels,
                                e,
                            )
                            .await;
                            break 'poller_loop;
                        }

                        // counting only iterations which return at least one canister message
                        polling_iteration += 1;
                    }


                    // pin a new asynchronous operation so that it can be restarted in the next select! iteration and continued in the following ones
                    get_messages_operation.set(self.get_canister_updates(message_nonce, polling_iteration, first_client_principal));
                },
            }
        }
    }

    async fn get_canister_updates(
        &self,
        message_nonce: u64,
        polling_iteration: u64,
        first_client_principal: ClientPrincipal,
    ) -> Option<CanisterGetMessagesWithEvents> {
        let mut poller_events = PollerEvents::new(
            Some(EventsReference::Iteration(polling_iteration)),
            EventsCollectionType::PollerStatus,
            PollerEventsMetrics::default(),
        );
        poller_events.metrics.set_start_polling();
        sleep(self.polling_interval_ms).await;
        // get messages to be relayed to clients from canister (starting from 'message_nonce')
        let mut canister_result = canister_methods::ws_get_messages(
            &self.agent,
            &self.canister_id,
            CanisterWsGetMessagesArguments {
                nonce: message_nonce,
            },
        )
        .await
        .ok()?;
        poller_events.metrics.set_received_messages();

        filter_canister_messages(
            &mut canister_result.messages,
            message_nonce,
            first_client_principal,
        );

        if canister_result.messages.len() > 0 {
            return Some((canister_result, poller_events));
        }
        None
    }
}

fn filter_canister_messages<'a>(
    messages: &'a mut Vec<CanisterOutputMessage>,
    message_nonce: u64,
    first_client_principal: ClientPrincipal,
) {
    if message_nonce == 0 {
        // if the poller just started (message_nonce == 0), the canister might have already had other messages in the queue which we should not send to the clients
        // therefore, starting from the last message polled, we relay the open message of type CanisterServiceMessage::OpenMessage for each connected client
        // message_nonce has to be set to the nonce of the last open message pollled in this iteration so that in the next iteration we can poll from there
        filter_messages_of_first_polling_iteration(messages, first_client_principal);
    }
    // this is not the first polling iteration and therefore the poller queried the canister starting from the nonce of the last message of the previous polling iteration
    // therefore, all the received messages are new and have to be relayed to the respective client handlers
}

/// Finds the response to the open message of the client that started the poller.
/// Returns all the messages following this message (inclusive).
fn filter_messages_of_first_polling_iteration<'a>(
    messages: &'a mut Vec<CanisterOutputMessage>,
    first_client_principal: ClientPrincipal,
) {
    // the filter assumes that, if the response to the open message of the first client that connects after the gateway reboots is not (yet) present,
    // the polled messages (which are all old) do not contain the result to a previous open message that the same client sent before the gateway rebooted

    // if this is not the case, the filter will return all the messages from the result of the open message of the client which reconnects first
    // however, this open message is old as it was sent by the client before the gateway rebooted and this is mistaken with
    // the result of the new open message has not yet been pushed to the message queue of the canister (and thus not polled)

    // TODO: figure out how to remove this assumption
    let len_before_filter = messages.len();
    messages.reverse();
    let mut keep = true;
    messages.retain(|canister_output_message| {
        if keep {
            let websocket_message: WebsocketMessage = from_slice(&canister_output_message.content)
                .expect("content of canister_output_message is not of type WebsocketMessage");
            if websocket_message.is_service_message {
                let canister_service_message = decode_one(&websocket_message.content)
                    .expect("content of websocket_message is not of type CanisterServiceMessage");
                if let CanisterServiceMessage::OpenMessage(CanisterOpenMessageContent {
                    client_principal,
                }) = canister_service_message
                {
                    if client_principal == first_client_principal {
                        keep = false;
                    }
                }
            }
            return true;
        }
        false
    });
    // in case the response to the open message to the first client that connects is not found, all messages have to be discarded
    // as they correspond to messages sent before the gateway rebooted
    if keep == true {
        messages.retain(|_m| false);
    }
    messages.reverse();
    trace!(
        "Filtered out {} polled messages",
        len_before_filter - messages.len()
    );
}

async fn process_queues(
    clients_message_queues: &mut HashMap<ClientPrincipal, Vec<CanisterToClientMessage>>,
    client_channels: &HashMap<ClientPrincipal, Sender<IcWsConnectionUpdate>>,
) {
    let mut handles = Vec::new();
    clients_message_queues.retain(|client_principal, message_queue| {
        if let Some(client_channel_tx) = client_channels.get(&client_principal) {
            // once a client channel is received, messages for that client will not be put in the queue anymore (until that client disconnects)
            // thus the respective queue does not need to be retained
            // relay all the messages previously received for the corresponding client
            let client_channel_tx = client_channel_tx.clone();
            let message_queue = message_queue.to_owned();
            let handle = tokio::spawn(async move {
                // make sure that messages are delivered to each client in the order defined by their sequence numbers
                for m in message_queue {
                    warn!("Processing message with key: {:?} from queue", m.key);
                    if let Err(e) = client_channel_tx
                        .send(IcWsConnectionUpdate::Message(m))
                        .await
                    {
                        error!("Client's thread terminated: {}", e);
                    }
                }
            });
            handles.push(handle);
            return false;
        }
        // if the client channel has not been received yet, keep the messages in the queue
        true
    });
    // the tasks must be awaited so that messages in queue are relayed before newly polled messages
    for handle in handles {
        let (_,) = join!(handle);
    }
}

async fn signal_termination_and_cleanup(
    poller_to_main_channel: &mut Sender<TerminationInfo>,
    canister_id: Principal,
    client_channels: &HashMap<ClientPrincipal, Sender<IcWsConnectionUpdate>>,
    e: String,
) {
    // let the main task know that this poller will terminate due to a CDK error
    signal_poller_task_termination(
        poller_to_main_channel,
        TerminationInfo::CdkError(canister_id),
    )
    .await;
    // let each client connection handler task connected to this poller know that the poller will terminate
    // and thus they also have to close the WebSocket connection and terminate
    for client_channel_tx in client_channels.values() {
        if let Err(channel_err) = client_channel_tx
            .send(IcWsConnectionUpdate::Error(format!(
                "Terminating poller task due to error: {}",
                e
            )))
            .await
        {
            error!("Client's thread terminated: {}", channel_err);
        }
    }
}

async fn signal_poller_task_termination(
    channel: &mut Sender<TerminationInfo>,
    info: TerminationInfo,
) {
    if let Err(e) = channel.send(info).await {
        error!(
            "Receiver has been dropped on the pollers connection manager's side. Error: {:?}",
            e
        );
    }
}

async fn sleep(millis: u64) {
    tokio::time::sleep(Duration::from_millis(millis)).await;
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use crate::canister_methods::{
        CanisterAckMessageContent, CanisterOpenMessageContent, CanisterOutputMessage,
        CanisterServiceMessage, ClientPrincipal, WebsocketMessage,
    };
    use crate::canister_poller::{
        filter_canister_messages, filter_messages_of_first_polling_iteration,
        CanisterToClientMessage, IcWsConnectionUpdate, PollerChannelsPollerEnds, TerminationInfo,
    };
    use crate::events_analyzer::Events;
    use crate::messages_demux::relay_message;
    use candid::{encode_one, Principal};
    use serde::Serialize;
    use serde_cbor::{from_slice, Serializer};
    use tokio::sync::mpsc::{self, Receiver, Sender};

    use super::{process_queues, PollerToClientChannelData};

    fn init_poller() -> (
        Sender<IcWsConnectionUpdate>,
        Receiver<IcWsConnectionUpdate>,
        HashMap<ClientPrincipal, Sender<IcWsConnectionUpdate>>,
        PollerChannelsPollerEnds,
        HashMap<ClientPrincipal, Vec<CanisterToClientMessage>>,
        Receiver<Box<dyn Events + Send>>,
        Sender<PollerToClientChannelData>,
        Receiver<TerminationInfo>,
    ) {
        let (message_for_client_tx, message_for_client_rx): (
            Sender<IcWsConnectionUpdate>,
            Receiver<IcWsConnectionUpdate>,
        ) = mpsc::channel(100);

        let client_channels: HashMap<ClientPrincipal, Sender<IcWsConnectionUpdate>> =
            HashMap::new();

        let (events_channel_tx, events_channel_rx) = mpsc::channel(100);

        let (
            poller_channel_for_client_channel_sender_tx,
            poller_channel_for_client_channel_sender_rx,
        ) = mpsc::channel(100);

        let (poller_channel_for_completion_tx, poller_channel_for_completion_rx): (
            Sender<TerminationInfo>,
            Receiver<TerminationInfo>,
        ) = mpsc::channel(100);

        let poller_channels_poller_ends = PollerChannelsPollerEnds::new(
            poller_channel_for_client_channel_sender_rx,
            poller_channel_for_completion_tx,
            events_channel_tx.clone(),
        );

        let clients_message_queues: HashMap<ClientPrincipal, Vec<CanisterToClientMessage>> =
            HashMap::new();

        (
            message_for_client_tx,
            message_for_client_rx,
            client_channels,
            poller_channels_poller_ends,
            clients_message_queues,
            events_channel_rx,
            poller_channel_for_client_channel_sender_tx,
            poller_channel_for_completion_rx,
        )
    }

    fn cbor_serialize<T: Serialize>(m: T) -> Vec<u8> {
        let mut bytes = Vec::new();
        let mut serializer = Serializer::new(&mut bytes);
        serializer.self_describe().unwrap();
        m.serialize(&mut serializer).unwrap();
        bytes
    }

    fn mock_open_message(client_principal: Principal) -> CanisterServiceMessage {
        CanisterServiceMessage::OpenMessage(CanisterOpenMessageContent { client_principal })
    }

    fn mock_ack_message() -> CanisterServiceMessage {
        CanisterServiceMessage::AckMessage(CanisterAckMessageContent {
            last_incoming_sequence_num: 0,
        })
    }

    fn mock_websocket_service_message(
        content: CanisterServiceMessage,
        client_principal: Principal,
        sequence_num: u64,
    ) -> WebsocketMessage {
        WebsocketMessage {
            client_principal,
            sequence_num,
            timestamp: 0,
            is_service_message: true,
            content: encode_one(content).unwrap(),
        }
    }

    fn mock_websocket_message(client_principal: Principal, sequence_num: u64) -> WebsocketMessage {
        WebsocketMessage {
            client_principal,
            sequence_num,
            timestamp: 0,
            is_service_message: false,
            content: vec![],
        }
    }

    fn mock_canister_output_message(
        content: WebsocketMessage,
        client_principal: Principal,
    ) -> CanisterOutputMessage {
        CanisterOutputMessage {
            client_principal,
            key: String::from("gateway_uid_0"),
            content: cbor_serialize(content),
        }
    }

    fn canister_open_message(
        client_principal: Principal,
        sequence_num: u64,
    ) -> CanisterOutputMessage {
        let open_message = mock_open_message(client_principal);
        let websocket_service_message =
            mock_websocket_service_message(open_message, client_principal, sequence_num);
        mock_canister_output_message(websocket_service_message, client_principal)
    }

    fn canister_ack_message(
        client_principal: Principal,
        sequence_num: u64,
    ) -> CanisterOutputMessage {
        let ack_message = mock_ack_message();
        let websocket_service_message =
            mock_websocket_service_message(ack_message, client_principal, sequence_num);
        mock_canister_output_message(websocket_service_message, client_principal)
    }

    fn canister_output_message(
        client_principal: Principal,
        sequence_num: u64,
    ) -> CanisterOutputMessage {
        let websocket_message = mock_websocket_message(client_principal, sequence_num);
        mock_canister_output_message(websocket_message, client_principal)
    }

    fn mock_messages_to_be_filtered() -> Vec<CanisterOutputMessage> {
        let old_client_principal = Principal::from_text("aaaaa-aa").unwrap();
        let reconnecting_client_principal =
            Principal::from_text("2chl6-4hpzw-vqaaa-aaaaa-c").unwrap();
        let new_client_principal =
            Principal::from_text("ygoe7-xpj6n-24gsd-zksfw-2mywm-xfyop-yvlsp-ctlwa-753xv-wz6rk-uae")
                .unwrap();

        let mut messages = Vec::new();

        // this message should be filtered out as it was sent before the gateway rebooted
        let canister_message = canister_output_message(old_client_principal, 10);
        messages.push(canister_message);

        // this message should be filtered out as it was sent before the gateway rebooted
        let canister_message = canister_open_message(reconnecting_client_principal, 0);
        messages.push(canister_message);

        // this message should be filtered out as it was sent before the gateway rebooted
        let canister_message = canister_ack_message(old_client_principal, 11);
        messages.push(canister_message);

        // this message should be filtered out as it was sent before the gateway rebooted
        let canister_message = canister_output_message(reconnecting_client_principal, 1);
        messages.push(canister_message);

        // this message should be filtered out as it was sent before the gateway rebooted
        let canister_message = canister_output_message(reconnecting_client_principal, 2);
        messages.push(canister_message);

        // this message should be filtered out as it was sent before the gateway rebooted
        let canister_message = canister_output_message(reconnecting_client_principal, 3);
        messages.push(canister_message);

        // this message should be filtered out as it was sent before the gateway rebooted
        let canister_message = canister_ack_message(reconnecting_client_principal, 4);
        messages.push(canister_message);

        // this message should be filtered out as it was sent before the gateway rebooted
        let canister_message = canister_output_message(old_client_principal, 12);
        messages.push(canister_message);

        // the gateway reboots and therefore all the previous connections are closed
        // client 2chl6-4hpzw-vqaaa-aaaaa-c reconnects
        // new client ygoe7-xpj6n-24gsd-zksfw-2mywm-xfyop-yvlsp-ctlwa-753xv-wz6rk-uae connects

        // this message should not be filtered out as it is the open message sent by the first client that (re)connects after the gateway reboots
        let canister_message = canister_open_message(reconnecting_client_principal, 0);
        messages.push(canister_message);

        // this message should not be filtered out as it was sent after the gateway rebooted
        let canister_message = canister_output_message(reconnecting_client_principal, 1);
        messages.push(canister_message);

        // this message should not be filtered out as it was sent after the gateway rebooted
        let canister_message = canister_output_message(reconnecting_client_principal, 2);
        messages.push(canister_message);

        // this message should not be filtered out as it was sent after the gateway rebooted
        let canister_message = canister_open_message(new_client_principal, 0);
        messages.push(canister_message);

        // this message should not be filtered out as it was sent after the gateway rebooted
        let canister_message = canister_output_message(new_client_principal, 1);
        messages.push(canister_message);

        messages
    }

    fn mock_all_old_messages_to_be_filtered() -> Vec<CanisterOutputMessage> {
        let old_client_principal = Principal::from_text("aaaaa-aa").unwrap();
        let reconnecting_client_principal =
            Principal::from_text("2chl6-4hpzw-vqaaa-aaaaa-c").unwrap();

        let mut messages = Vec::new();

        // this message should be filtered out as it was sent before the gateway rebooted
        let canister_message = canister_output_message(old_client_principal, 10);
        messages.push(canister_message);

        // this message should be filtered out as it was sent before the gateway rebooted
        let canister_message = canister_ack_message(old_client_principal, 11);
        messages.push(canister_message);

        // this message should be filtered out as it was sent before the gateway rebooted
        let canister_message = canister_output_message(reconnecting_client_principal, 1);
        messages.push(canister_message);

        // this message should be filtered out as it was sent before the gateway rebooted
        let canister_message = canister_output_message(reconnecting_client_principal, 2);
        messages.push(canister_message);

        // this message should be filtered out as it was sent before the gateway rebooted
        let canister_message = canister_output_message(reconnecting_client_principal, 3);
        messages.push(canister_message);

        // this message should be filtered out as it was sent before the gateway rebooted
        let canister_message = canister_ack_message(reconnecting_client_principal, 4);
        messages.push(canister_message);

        // this message should be filtered out as it was sent before the gateway rebooted
        let canister_message = canister_output_message(old_client_principal, 12);
        messages.push(canister_message);

        // the gateway reboots and therefore all the previous connections are closed
        // client 2chl6-4hpzw-vqaaa-aaaaa-c will reconnect but its open message is not ready for this polling iteration

        // all messages should therefore be filtered out
        messages
    }

    fn mock_ordered_messages(
        client_principal: Principal,
        start_sequence_number: u64,
    ) -> Vec<CanisterOutputMessage> {
        let mut sequence_number = start_sequence_number;

        let mut messages = Vec::new();
        while sequence_number < 10 {
            let canister_message = canister_output_message(client_principal, sequence_number);
            messages.push(canister_message);
            sequence_number += 1;
        }
        messages
    }

    #[tokio::test()]
    /// Simulates the case in which polled messages are filtered before being relayed.
    /// The messages that are not fltered out should be relayed in the same order as when polled.
    async fn should_return_filtered_messages_in_order() {
        let client_principal = Principal::from_text("2chl6-4hpzw-vqaaa-aaaaa-c").unwrap();

        let mut messages = mock_messages_to_be_filtered();
        filter_messages_of_first_polling_iteration(&mut messages, client_principal);
        assert_eq!(messages.len(), 5);

        let mut expected_sequence_number = 0;
        for canister_output_message in messages {
            let websocket_message: WebsocketMessage = from_slice(&canister_output_message.content)
                .expect("content of canister_output_message is not of type WebsocketMessage");
            if websocket_message.client_principal == client_principal {
                assert_eq!(websocket_message.sequence_num, expected_sequence_number);
                expected_sequence_number += 1;
            }
        }
        assert_eq!(expected_sequence_number, 3);
    }

    #[tokio::test()]
    /// Simulates the case in which polled messages are all from before the gateway rebooted.
    /// The messages that are not fltered out should be relayed in the same order as when polled.
    async fn should_filter_out_all_messages() {
        let client_principal = Principal::from_text("2chl6-4hpzw-vqaaa-aaaaa-c").unwrap();

        let mut messages = mock_all_old_messages_to_be_filtered();
        filter_messages_of_first_polling_iteration(&mut messages, client_principal);
        assert_eq!(messages.len(), 0);
    }

    #[tokio::test()]
    /// Simulates the case in which the poller starts and the canister's queue contains some old messages.
    /// Relays only open messages for the connected clients.
    async fn filterprocess_canister_messages() {
        let (
            message_for_client_tx,
            mut message_for_client_rx,
            mut client_channels,
            poller_channels_poller_ends,
            mut clients_message_queues,
            // the following have to be returned in order not to drop them
            _events_channel_rx,
            _poller_channel_for_client_channel_sender_tx,
            _poller_channel_for_completion_rx,
        ) = init_poller();

        let reconnecting_client_principal =
            Principal::from_text("2chl6-4hpzw-vqaaa-aaaaa-c").unwrap();
        client_channels.insert(reconnecting_client_principal, message_for_client_tx.clone());
        // messages from 2chl6-4hpzw-vqaaa-aaaaa-c must be relayed as the client is registered in the poller

        // simulating the case in which the poller did not yet receive the client channel for client ygoe7-xpj6n-24gsd-zksfw-2mywm-xfyop-yvlsp-ctlwa-753xv-wz6rk-uae
        // messages from ygoe7-xpj6n-24gsd-zksfw-2mywm-xfyop-yvlsp-ctlwa-753xv-wz6rk-uae should be pushed to the queue

        let mut messages = mock_messages_to_be_filtered();
        let mut message_nonce = 0;
        filter_canister_messages(&mut messages, message_nonce, reconnecting_client_principal);
        assert_eq!(messages.len(), 5);

        let mut received = 0;
        let mut queued = 0;
        for canister_output_message in messages {
            let client_principal = canister_output_message.client_principal;
            let canister_to_client_message = CanisterToClientMessage {
                key: canister_output_message.key,
                content: canister_output_message.content,
                cert: Vec::new(),
                tree: Vec::new(),
            };
            relay_message(
                client_principal,
                canister_to_client_message,
                &client_channels,
                &poller_channels_poller_ends,
                &mut clients_message_queues,
                &mut message_nonce,
            )
            .await
            .unwrap();

            match message_for_client_rx.try_recv() {
                Ok(update) => {
                    if let IcWsConnectionUpdate::Message(m) = update {
                        // counts the messages relayed should only be for client 2chl6-4hpzw-vqaaa-aaaaa-c
                        // as it is the only one registered in the poller
                        let websocket_message: WebsocketMessage = from_slice(&m.content)
                            .expect("content must be of type WebsocketMessage");
                        // only client 2chl6-4hpzw-vqaaa-aaaaa-c should be registered in poller
                        assert_eq!(
                            websocket_message.client_principal,
                            reconnecting_client_principal
                        );
                        received += 1
                    } else {
                        panic!("updates must be of variant Message")
                    }
                },
                Err(_) => queued += 1,
            }
        }
        let expected_received = 3; // number of messages for 2chl6-4hpzw-vqaaa-aaaaa-c in mock_messages_to_be_filtered after gateway reboots
        assert_eq!(received, expected_received);
        let expected_queued = 2; // number of messages for ygoe7-xpj6n-24gsd-zksfw-2mywm-xfyop-yvlsp-ctlwa-753xv-wz6rk-uae
        assert_eq!(queued, expected_queued);

        let mut messages = mock_messages_to_be_filtered();
        // here message_nonce is > 0, so messages will not be filtered
        filter_canister_messages(&mut messages, message_nonce, reconnecting_client_principal);
        assert_eq!(messages.len(), 13);
    }

    #[tokio::test()]
    /// Simulates the case in which the gateway polls a message for a client that is not yet registered in the poller.
    /// Stores the message in the queue so that it can be processed later.
    async fn should_push_message_to_queue() {
        let (
            _message_for_client_tx,
            _message_for_client_rx,
            client_channels,
            poller_channels_poller_ends,
            mut clients_message_queues,
            // the following have to be returned in order not to drop them
            _events_channel_rx,
            _poller_channel_for_client_channel_sender_tx,
            _poller_channel_for_completion_rx,
        ) = init_poller();

        let client_principal = Principal::from_text("2chl6-4hpzw-vqaaa-aaaaa-c").unwrap();
        let sequence_number = 0;
        let canister_output_message = canister_open_message(client_principal, sequence_number);
        let mut message_nonce = 0;

        let canister_to_client_message = CanisterToClientMessage {
            key: canister_output_message.key,
            content: canister_output_message.content,
            cert: Vec::new(),
            tree: Vec::new(),
        };
        relay_message(
            client_principal,
            canister_to_client_message,
            &client_channels,
            &poller_channels_poller_ends,
            &mut clients_message_queues,
            &mut message_nonce,
        )
        .await
        .unwrap();

        assert_eq!(clients_message_queues.len(), 1);
    }

    #[tokio::test()]
    /// Simulates the case in which there is a message in the queue for a client that is connected.
    /// Relays the message to the client and empties the queue.
    async fn should_process_message_in_queue() {
        let (
            message_for_client_tx,
            mut message_for_client_rx,
            mut client_channels,
            _poller_channels_poller_ends,
            mut clients_message_queues,
            // the following have to be returned in order not to drop them
            _events_channel_rx,
            _poller_channel_for_client_channel_sender_tx,
            _poller_channel_for_completion_rx,
        ) = init_poller();

        let client_principal = Principal::from_text("2chl6-4hpzw-vqaaa-aaaaa-c").unwrap();
        let sequence_number = 0;
        let canister_output_message = canister_open_message(client_principal, sequence_number);
        let client_principal = canister_output_message.client_principal;
        let m = CanisterToClientMessage {
            key: canister_output_message.key.clone(),
            content: canister_output_message.content,
            cert: Vec::new(),
            tree: Vec::new(),
        };
        clients_message_queues.insert(client_principal, vec![m]);

        // simulates the client being registered in the poller
        client_channels.insert(client_principal, message_for_client_tx);

        process_queues(&mut clients_message_queues, &client_channels).await;

        if let None = message_for_client_rx.recv().await {
            panic!("should receive message");
        }

        assert_eq!(clients_message_queues.len(), 0);
    }

    #[tokio::test()]
    /// Simulates the case in which there is a message in the queue for a client that is not yet connected.
    /// Keeps the message in the queue.
    async fn should_keep_message_in_queue() {
        let (
            _message_for_client_tx,
            _message_for_client_rx,
            client_channels,
            _poller_channels_poller_ends,
            mut clients_message_queues,
            // the following have to be returned in order not to drop them
            _events_channel_rx,
            _poller_channel_for_client_channel_sender_tx,
            _poller_channel_for_completion_rx,
        ) = init_poller();

        let client_principal = Principal::from_text("2chl6-4hpzw-vqaaa-aaaaa-c").unwrap();
        let sequence_number = 0;
        let canister_output_message = canister_open_message(client_principal, sequence_number);
        let client_principal = canister_output_message.client_principal;
        let m = CanisterToClientMessage {
            key: canister_output_message.key.clone(),
            content: canister_output_message.content,
            cert: Vec::new(),
            tree: Vec::new(),
        };
        clients_message_queues.insert(client_principal, vec![m]);

        process_queues(&mut clients_message_queues, &client_channels).await;

        assert_eq!(clients_message_queues.len(), 1);
    }

    #[tokio::test()]
    /// Simulates the case in which there are multiple messages in the queue for a client that is connected.
    /// Relays the messages to the client in ascending order specified by the sequence number.
    async fn should_receive_messages_from_queue_in_order() {
        let (
            message_for_client_tx,
            mut message_for_client_rx,
            mut client_channels,
            _poller_channels_poller_ends,
            mut clients_message_queues,
            // the following have to be returned in order not to drop them
            _events_channel_rx,
            _poller_channel_for_client_channel_sender_tx,
            _poller_channel_for_completion_rx,
        ) = init_poller();

        let mut messages = Vec::new();
        let client_principal = Principal::from_text("2chl6-4hpzw-vqaaa-aaaaa-c").unwrap();
        let start_sequence_number = 0;
        for canister_output_message in
            mock_ordered_messages(client_principal, start_sequence_number)
        {
            let m = CanisterToClientMessage {
                key: canister_output_message.key.clone(),
                content: canister_output_message.content,
                cert: Vec::new(),
                tree: Vec::new(),
            };
            messages.push(m);
        }

        let count_messages = messages.len() as u64;
        clients_message_queues.insert(client_principal, messages);

        // simulates the client being registered in the poller
        client_channels.insert(client_principal, message_for_client_tx);

        process_queues(&mut clients_message_queues, &client_channels).await;

        let mut expected_sequence_number = 0;
        while let Ok(IcWsConnectionUpdate::Message(m)) = message_for_client_rx.try_recv() {
            let websocket_message: WebsocketMessage = from_slice(&m.content)
                .expect("content of canister_output_message is not of type WebsocketMessage");
            assert_eq!(websocket_message.sequence_num, expected_sequence_number);
            expected_sequence_number += 1;
        }

        // make sure that all messages are received
        assert_eq!(count_messages, expected_sequence_number);
        // make sure that no messages are pushed into the queue
        assert_eq!(clients_message_queues.len(), 0);
    }

    #[tokio::test()]
    /// Simulates the case in which the gateway polls multiple messages for a client that is connected.
    /// Relays the messages to the client in ascending order specified by the sequence number.
    async fn should_relay_polled_messages_in_order() {
        let (
            message_for_client_tx,
            mut message_for_client_rx,
            mut client_channels,
            poller_channels_poller_ends,
            mut clients_message_queues,
            // the following have to be returned in order not to drop them
            _events_channel_rx,
            _poller_channel_for_client_channel_sender_tx,
            _poller_channel_for_completion_rx,
        ) = init_poller();

        let client_principal = Principal::from_text("2chl6-4hpzw-vqaaa-aaaaa-c").unwrap();
        client_channels.insert(client_principal, message_for_client_tx);

        let start_sequence_number = 0;
        let messages = mock_ordered_messages(client_principal, start_sequence_number);
        let count_messages = messages.len() as u64;
        let mut message_nonce = 0;
        for canister_output_message in messages {
            let canister_to_client_message = CanisterToClientMessage {
                key: canister_output_message.key,
                content: canister_output_message.content,
                cert: Vec::new(),
                tree: Vec::new(),
            };
            relay_message(
                client_principal,
                canister_to_client_message,
                &client_channels,
                &poller_channels_poller_ends,
                &mut clients_message_queues,
                &mut message_nonce,
            )
            .await
            .unwrap();
        }

        let mut expected_sequence_number = 0;
        while let Ok(IcWsConnectionUpdate::Message(m)) = message_for_client_rx.try_recv() {
            let websocket_message: WebsocketMessage = from_slice(&m.content)
                .expect("content of canister_output_message is not of type WebsocketMessage");
            assert_eq!(websocket_message.sequence_num, expected_sequence_number);
            expected_sequence_number += 1;
        }

        // make sure that all messages are received
        assert_eq!(count_messages, expected_sequence_number);
        // make sure that no messages are pushed into the queue
        assert_eq!(clients_message_queues.len(), 0);
    }

    #[tokio::test()]
    /// Simulates the case in which the gateway polls multiple messages for a client that is connected while there are already multiple messages in the queue.
    /// Relays the messages to the client in ascending order specified by the sequence number.
    async fn should_relay_polled_messages_in_order_after_processing_queue() {
        let (
            message_for_client_tx,
            mut message_for_client_rx,
            mut client_channels,
            poller_channels_poller_ends,
            mut clients_message_queues,
            // the following have to be returned in order not to drop them
            _events_channel_rx,
            _poller_channel_for_client_channel_sender_tx,
            _poller_channel_for_completion_rx,
        ) = init_poller();

        let client_principal = Principal::from_text("2chl6-4hpzw-vqaaa-aaaaa-c").unwrap();
        client_channels.insert(client_principal, message_for_client_tx);

        let mut messages_in_queue = Vec::new();
        let client_principal = Principal::from_text("2chl6-4hpzw-vqaaa-aaaaa-c").unwrap();
        let start_sequence_number = 0;
        for canister_output_message in
            mock_ordered_messages(client_principal, start_sequence_number)
        {
            let m = CanisterToClientMessage {
                key: canister_output_message.key.clone(),
                content: canister_output_message.content,
                cert: Vec::new(),
                tree: Vec::new(),
            };
            messages_in_queue.push(m);
        }

        let count_messages_in_queue = messages_in_queue.len() as u64;
        clients_message_queues.insert(client_principal, messages_in_queue);

        process_queues(&mut clients_message_queues, &client_channels).await;

        let start_sequence_number = count_messages_in_queue;
        let polled_messages = mock_ordered_messages(client_principal, start_sequence_number);
        let count_polled_messages = polled_messages.len() as u64;
        let mut message_nonce = 0;
        for canister_output_message in polled_messages {
            let client_principal = canister_output_message.client_principal;
            let canister_to_client_message = CanisterToClientMessage {
                key: canister_output_message.key,
                content: canister_output_message.content,
                cert: Vec::new(),
                tree: Vec::new(),
            };
            relay_message(
                client_principal,
                canister_to_client_message,
                &client_channels,
                &poller_channels_poller_ends,
                &mut clients_message_queues,
                &mut message_nonce,
            )
            .await
            .unwrap();
        }

        let mut expected_sequence_number = 0;
        while let Ok(IcWsConnectionUpdate::Message(m)) = message_for_client_rx.try_recv() {
            let websocket_message: WebsocketMessage = from_slice(&m.content)
                .expect("content of canister_output_message is not of type WebsocketMessage");
            assert_eq!(websocket_message.sequence_num, expected_sequence_number);
            expected_sequence_number += 1;
        }

        // make sure that all messages are received
        assert_eq!(
            count_messages_in_queue + count_polled_messages,
            expected_sequence_number
        );
        // make sure that no messages are pushed into the queue
        assert_eq!(clients_message_queues.len(), 0);
    }
}
