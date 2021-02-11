use js_sys::Array;
use yew::{agent::{Dispatched ,Dispatcher}, worker::*};
use std::{cell::RefCell, collections::HashSet, convert::TryFrom};
use std::{rc::Rc, vec};
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::{
    MessageEvent, RtcConfiguration, RtcDataChannel, RtcDataChannelInit, RtcDataChannelState,
    RtcIceCandidateInit, RtcIceServer, RtcPeerConnection, RtcPeerConnectionIceEvent, RtcSdpType,
    RtcSessionDescription, RtcSessionDescriptionInit, RtcSignalingState, WebSocket,
};

use serde::{Deserialize, Serialize};
use crate::{components::chat_message::{ChatMessage, SenderType}, utils::{
    participants::Participants,
    socket::{Candidate, Room, SDPMessage, SignalingMessage, SocketMessage},
}};

type BoxDynJsValue = Box<dyn FnMut(JsValue)>;
type BoxDynMessageEvent = Box<dyn FnMut(MessageEvent)>;
type BoxDynEvent<T> = Box<dyn FnMut(T)>;

pub struct EventBus {
    link: AgentLink<EventBus>,
    subscribers: HashSet<HandlerId>,
}
#[derive(Serialize, Deserialize, Debug)]
pub enum Request {
    EventBusMsg(ChatMessage),
}

impl Agent for EventBus {
    type Reach = Context;
    type Message = ();
    type Input = Request;
    type Output = ChatMessage;

    fn create(link: AgentLink<Self>) -> Self {
        Self {
            link,
            subscribers: HashSet::new(),
        }
    }

    fn update(&mut self, _msg: Self::Message) {}

    fn handle_input(&mut self, msg: Self::Input, _id: HandlerId) {
        match msg {
            Request::EventBusMsg(s) => {
                for sub in self.subscribers.iter() {
                    self.link.respond(*sub, s.clone());
                }
            }
        }
    }

    fn connected(&mut self, id: HandlerId) {
        self.subscribers.insert(id);
    }

    fn disconnected(&mut self, id: HandlerId) {
        self.subscribers.remove(&id);
    }
}

pub struct WebRTC {
    // https://rustwasm.github.io/wasm-bindgen/api/web_sys/struct.RtcPeerConnection.html
    pub connection: RtcPeerConnection,
    room: Option<String>,
    signaling_channel_opened: bool,
    is_negotiating: bool,
    candidates_buffer: Vec<RtcIceCandidateInit>,
    data_channel: Option<RtcDataChannel>,
    socket: WebSocket,
    event_bus: Dispatcher<EventBus>
}

impl WebRTC {
    pub fn new() -> Self {
        let mut ice_server = RtcIceServer::new();
        ice_server.urls(&JsValue::from_str("stun:stun.l.google.com:19302"));

        let mut configuration = RtcConfiguration::new();
        configuration.ice_servers(&Array::of1(&ice_server));
        let peer_connection = RtcPeerConnection::new_with_configuration(&configuration)
            .expect("Cannot create a Peer Connection");

        let socket = WebSocket::new("wss://glacial-beyond-33808.herokuapp.com").unwrap();

        // Is equivalent to onConnect in JS
        let onopen_callback = Closure::wrap(Box::new(move |_| {
            log::info!("socket opened");
        }) as BoxDynJsValue);
        socket.set_onopen(Some(onopen_callback.as_ref().unchecked_ref()));
        onopen_callback.forget();

        let onclose_callback = Closure::wrap(Box::new(move |_| {
            log::info!("socket closed");
        }) as BoxDynJsValue);
        socket.set_onclose(Some(onclose_callback.as_ref().unchecked_ref()));
        onclose_callback.forget();

        Self {
            connection: peer_connection,
            room: None,
            is_negotiating: false,
            candidates_buffer: vec![],
            signaling_channel_opened: false,
            data_channel: None,
            socket,
            event_bus: EventBus::dispatcher(),
        }
    }

    fn set_is_negotiating(&mut self, value: bool) {
        self.is_negotiating = value;
    }

    pub fn connect(web_rtc: Rc<RefCell<WebRTC>>, participants: Participants) {
        let on_message_clone = web_rtc.clone();
        let on_message_callback = Closure::wrap(Box::new(move |message: MessageEvent| {
            let message = SocketMessage::try_from(message);
            match message {
                Ok(parsed) => WebRTC::handle_message(on_message_clone.clone(), parsed),
                Err(error) => log::error!("Oh No: {:?}", error),
            };
        }) as BoxDynMessageEvent);
        {
            // To avoid multiple borrow in the same time we borrow in this limited scope
            web_rtc
                .clone()
                .as_ref()
                .borrow()
                .socket
                .set_onmessage(Some(on_message_callback.as_ref().unchecked_ref()));
            on_message_callback.forget();
        }

        let on_ice_cloned = web_rtc.clone();
        let on_ice_candidate_callback =
            Closure::wrap(Box::new(move |event: RtcPeerConnectionIceEvent| {
                log::info!("ICE 1: Send ice_candidate to signaling server");
                if let Some(candidate) = event.candidate() {
                    if !candidate.candidate().is_empty() {
                        let signal_message_from_client = SocketMessage::SignalMessageFromClient {
                            content: SignalingMessage::ICECandidate {
                                message: Candidate {
                                    candidate: candidate.candidate(),
                                    sdp_mid: candidate.sdp_mid().unwrap(),
                                    sdp_m_line_index: candidate.sdp_m_line_index().unwrap(),
                                },
                            },
                        };

                        let json_from_client_message =
                            serde_json::to_string(&signal_message_from_client).unwrap();
                        let send_res = on_ice_cloned
                            .as_ref()
                            .borrow()
                            .socket
                            .send_with_str(json_from_client_message.as_ref());
                        if let Err(ex) = send_res {
                            log::error!("Could not execute ice candidate callback {:?}", ex)
                        }
                    }
                }
            }) as BoxDynEvent<RtcPeerConnectionIceEvent>);
        let webrtc_signaling_clone = web_rtc.clone();
        let on_signaling_callback = Closure::wrap(Box::new(move |_: MessageEvent| {
            let new_value = webrtc_signaling_clone
                .as_ref()
                .borrow()
                .connection
                .signaling_state()
                == RtcSignalingState::Stable;
            webrtc_signaling_clone
                .as_ref()
                .borrow_mut()
                .set_is_negotiating(new_value);
        }) as BoxDynMessageEvent);

        let sdp_clone = web_rtc.clone();
        let send_sdp_callback = Closure::wrap(Box::new(move |_: JsValue| {
            log::info!("Step 3: On negociation needed, send offer to signaling server");
            let session_description = sdp_clone
                .as_ref()
                .borrow()
                .connection
                .local_description()
                .unwrap();
            let message_to_send = SocketMessage::SignalMessageFromClient {
                content: SignalingMessage::SDP {
                    message: SDPMessage::try_from(session_description).unwrap(),
                },
            };
            let message_to_send = serde_json::to_string(&message_to_send).unwrap();
            match sdp_clone
                .as_ref()
                .borrow()
                .socket
                .send_with_str(&message_to_send)
            {
                Ok(_) => (),
                Err(err) => log::error!("Error in sdp callback {:?}", err),
            };
        }) as BoxDynEvent<JsValue>);

        let on_negociation_success_clone = web_rtc.clone();
        let negociation_success_callback = Closure::wrap(Box::new(move |descriptor: JsValue| {
            log::info!("Step 2: On negociation needed, set_local_description");
            let description_init = RtcSessionDescriptionInit::try_from(descriptor).unwrap();
            let _ = on_negociation_success_clone
                .as_ref()
                .borrow()
                .connection
                .set_local_description(&description_init)
                .then(&send_sdp_callback);
        }) as BoxDynEvent<JsValue>);

        let on_negociation_needed_clone = web_rtc.clone();
        let on_negociation_needed_callback = Closure::wrap(Box::new(move |_: JsValue| {
            let mut borrow_mut = on_negociation_needed_clone.as_ref().borrow_mut();
            if !borrow_mut.is_negotiating {
                log::info!("Step 1: On negociation needed, create offer");
                borrow_mut.set_is_negotiating(true);

                let print_error_callback =
                    Closure::wrap(Box::new(|err| log::error!("{:?}", err)) as BoxDynJsValue);
                let _ = borrow_mut
                    .connection
                    .create_offer()
                    .then(&negociation_success_callback)
                    .catch(&print_error_callback);
            }
        }) as BoxDynJsValue);
        {
            // Creates a scope to avoid multiple borrow mut.
            web_rtc
                .as_ref()
                .borrow()
                .connection
                .set_onicecandidate(Some(on_ice_candidate_callback.as_ref().unchecked_ref()));
            on_ice_candidate_callback.forget();

            web_rtc
                .as_ref()
                .borrow()
                .connection
                .set_onsignalingstatechange(Some(on_signaling_callback.as_ref().unchecked_ref()));
            on_signaling_callback.forget();

            web_rtc
                .as_ref()
                .borrow()
                .connection
                .set_onnegotiationneeded(Some(
                    on_negociation_needed_callback.as_ref().unchecked_ref(),
                ));
            on_negociation_needed_callback.forget();

            // Send message in socket
            let new_user_message = SocketMessage::NewUser {
                content: participants.clone(),
            };
            let json_new_user_message = serde_json::to_string(&new_user_message).unwrap();
            let send_res = web_rtc
                .as_ref()
                .borrow()
                .socket
                .send_with_str(json_new_user_message.as_ref());
            match send_res {
                Ok(_) => (),
                Err(ex) => log::error!("Could not connect to websocket {:?}", ex),
            }
        }
    }

    pub fn send_message(web_rtc: Rc<RefCell<WebRTC>>, message: &str) {
        if let Some(data_channel) = &web_rtc.as_ref().borrow().data_channel {
            if data_channel.ready_state() == RtcDataChannelState::Open {
                match data_channel.send_with_str(message) {
                    Ok(_) => (),
                    Err(err) => log::error!("Could not send message {:?}", err),
                }
            }
        };
    }

    fn handle_message(web_rtc: Rc<RefCell<WebRTC>>, socket_message: SocketMessage) {
        match socket_message {
            SocketMessage::JoinedRoom { content } => {
                WebRTC::join_room(web_rtc, content);
            }
            SocketMessage::NewUser { .. } => {}
            SocketMessage::SignalMessageToClient {
                content: SignalingMessage::UserHere { message },
            } => {
                WebRTC::handle_user_here(web_rtc, message);
            }
            SocketMessage::SignalMessageToClient {
                content: SignalingMessage::ICECandidate { message },
            } => {
                WebRTC::handle_ice_candidate(web_rtc, message);
            }
            SocketMessage::SignalMessageToClient {
                content: SignalingMessage::SDP { message },
            } => {
                WebRTC::handle_sdp_message(web_rtc, message);
            }
            SocketMessage::SignalMessageFromClient { .. } => {}
        }
    }

    fn join_room(web_rtc: Rc<RefCell<WebRTC>>, content: Room) {
        (*web_rtc.as_ref().borrow_mut()).room = Some(content.room.clone());
    }

    fn handle_user_here(web_rtc: Rc<RefCell<WebRTC>>, signaling_id: u16) {
        let cloned_web_rtc = web_rtc.clone();
        let mut borrow_mut = cloned_web_rtc.as_ref().borrow_mut();
        if !borrow_mut.signaling_channel_opened {
            let current_room = &borrow_mut.room;
            let mut data_channel_init = RtcDataChannelInit::new();
            data_channel_init.negotiated(true);
            data_channel_init.id(signaling_id);
            let data_channel = borrow_mut
                .connection
                .create_data_channel_with_data_channel_dict(
                    &(current_room.as_ref().unwrap()),
                    &data_channel_init,
                );

            let cloned_on_message = cloned_web_rtc.clone();
            let on_message_data_channel_callback =
                Closure::wrap(Box::new(move |ev: MessageEvent| {
                    let mut on_message_borrowed = cloned_on_message.borrow_mut();
                    // TODO: Display this message as a YOU on the UI.
                    if let Some(message) = ev.data().as_string() {
                        // on_message_borrowed.messages.push(ChatMessage::new(SenderType::YOU, message));
                        on_message_borrowed.event_bus.send(Request::EventBusMsg(ChatMessage::new(SenderType::YOU, message)));
                    } else {
                        log::warn!("Received message error");
                    }
                }) as BoxDynMessageEvent);

            data_channel.set_onmessage(Some(
                on_message_data_channel_callback.as_ref().unchecked_ref(),
            ));
            on_message_data_channel_callback.forget();
            borrow_mut.data_channel = Some(data_channel);
        }
    }

    fn handle_ice_candidate(web_rtc: Rc<RefCell<WebRTC>>, candidate: Candidate) {
        let cloned_web_rtc = web_rtc.clone();

        let mut borrowed = cloned_web_rtc.as_ref().borrow_mut();
        let remote_description: Option<RtcSessionDescription> =
            borrowed.connection.remote_description();

        if remote_description.is_none() {
            let mut candidate_init = RtcIceCandidateInit::new(&candidate.candidate);
            candidate_init.sdp_m_line_index(Some(candidate.sdp_m_line_index));
            candidate_init.sdp_mid(Some(&candidate.sdp_mid));
            borrowed.candidates_buffer.push(candidate_init);
        } else {
            let mut candidate_init = RtcIceCandidateInit::new(&candidate.candidate);
            candidate_init.sdp_m_line_index(Some(candidate.sdp_m_line_index));
            candidate_init.sdp_mid(Some(&candidate.sdp_mid));
            let print_error_callback = Closure::wrap(Box::new(|err| {
                log::error!("remote description {:?}", err)
            }) as BoxDynJsValue);
            let print_success_callback = Closure::wrap(Box::new(|_| {}) as BoxDynJsValue);

            let _ = borrowed
                .connection
                .add_ice_candidate_with_opt_rtc_ice_candidate_init(Some(&candidate_init))
                .then(&print_success_callback)
                .catch(&print_error_callback);
            print_error_callback.forget();
            print_success_callback.forget();
        }
    }

    fn handle_sdp_message(web_rtc: Rc<RefCell<WebRTC>>, sdp_message: SDPMessage) {
        let description_init = RtcSessionDescriptionInit::try_from(sdp_message).unwrap();
        let clone = web_rtc.clone();

        let send_sdp_callback = Closure::wrap(Box::new(move |_: JsValue| {
            log::info!("Step 7: Handle SDP, send SDP answer");
            let borrow_mut = clone.borrow_mut();
            let message_to_send = SocketMessage::SignalMessageFromClient {
                content: SignalingMessage::SDP {
                    message: SDPMessage::try_from(
                        borrow_mut.connection.local_description().unwrap(),
                    )
                    .unwrap(),
                },
            };
            let message_to_send = serde_json::to_string(&message_to_send).unwrap();
            match borrow_mut.socket.send_with_str(&message_to_send) {
                Ok(_) => log::info!("binary message successfully sent"),
                Err(err) => log::error!("error sending message: {:?}", err),
            }
        }) as BoxDynEvent<JsValue>);

        let set_local_clone = web_rtc.clone();
        let set_local_description_callback = Closure::wrap(Box::new(move |descriptor: JsValue| {
            let sdp_message = descriptor.into_serde::<SDPMessage>().unwrap();
            let session_description_init =
                RtcSessionDescriptionInit::try_from(sdp_message).unwrap();
            let _ = set_local_clone
                .borrow()
                .connection
                .set_local_description(&session_description_init)
                .then(&send_sdp_callback);
            log::info!("Step 6: Handle SDP, set_local_description");
        }) as BoxDynEvent<JsValue>);

        let clone_remote_description_success = web_rtc.clone();
        let remote_description_success_callback = Closure::wrap(Box::new(move |_: JsValue| {
            if clone_remote_description_success
                .as_ref()
                .borrow()
                .connection
                .remote_description()
                .unwrap()
                .type_()
                == RtcSdpType::Offer
            {
                log::info!("Step 5: Handle SDP, create_answer");
                let _ = clone_remote_description_success
                    .as_ref()
                    .borrow()
                    .connection
                    .create_answer()
                    .then(&set_local_description_callback);
            }
            // send Queued Candidates
            for candidate in &clone_remote_description_success
                .as_ref()
                .borrow()
                .candidates_buffer
            {
                let _ = clone_remote_description_success
                    .as_ref()
                    .borrow()
                    .connection
                    .add_ice_candidate_with_opt_rtc_ice_candidate_init(Some(&candidate));
            }
        }) as BoxDynEvent<JsValue>);

        log::info!("Step 4: Handle SDP, set_remote_description");
        let clone_2 = web_rtc.clone();
        let _ = clone_2
            .as_ref()
            .borrow()
            .connection
            .set_remote_description(&description_init)
            .then(&remote_description_success_callback);
        remote_description_success_callback.forget();
    }
}
