use super::*;
use super::{capabilities::*, js_boundary::*, rtc::*};
use crate::error::{Error, Result};

pub(crate) struct BrowserRtcRegistry {
    sessions: Rc<RefCell<HashMap<String, BrowserRtcSession>>>,
}

struct BrowserRtcSession {
    peer: RtcPeerConnection,
    local_ice: LocalIceQueue,
    incoming_data_channels: Rc<RefCell<VecDeque<RtcDataChannel>>>,
    incoming_data_channel_waiters: Rc<RefCell<VecDeque<oneshot::Sender<RtcDataChannel>>>>,
    _ice_handler: Closure<dyn FnMut(RtcPeerConnectionIceEvent)>,
    _data_channel_handler: Closure<dyn FnMut(RtcDataChannelEvent)>,
}

impl Drop for BrowserRtcSession {
    fn drop(&mut self) {
        // Detach browser callbacks before their Rust closures are released.
        self.peer.set_onicecandidate(None);
        self.peer.set_ondatachannel(None);
        self.peer.close();
        self.local_ice.close();
    }
}

impl BrowserRtcRegistry {
    pub(crate) fn close_all(&self) {
        let sessions: Vec<_> = self.sessions.borrow().keys().cloned().collect();
        for key in sessions {
            let _ = self.try_close_peer_connection(&key);
        }
    }

    /// Selected ICE candidate pairs, read from the browser's live WebRTC stats.
    pub(crate) async fn path_diagnostics(&self) -> std::result::Result<serde_json::Value, JsValue> {
        let peers: Vec<_> = self
            .sessions
            .borrow()
            .iter()
            .take(16)
            .map(|(key, session)| (key.clone(), session.peer.clone()))
            .collect();
        let mut reports = Vec::new();
        for (session, peer) in peers {
            let report = JsFuture::from(peer.get_stats()).await?;
            let mut stats = Vec::<serde_json::Value>::new();
            if let Some(entries) = js_sys::try_iter(&report)? {
                for entry in entries.take(256) {
                    let pair = js_sys::Array::from(&entry?);
                    let text = js_sys::JSON::stringify(&pair.get(1))?
                        .as_string()
                        .ok_or_else(|| JsValue::from_str("invalid WebRTC statistics"))?;
                    stats.push(
                        serde_json::from_str(&text)
                            .map_err(|e| JsValue::from_str(&e.to_string()))?,
                    );
                }
            }
            let selected = stats
                .iter()
                .find(|s| s["type"] == "transport")
                .and_then(|s| s["selectedCandidatePairId"].as_str());
            let pair = selected.and_then(|id| stats.iter().find(|s| s["id"] == id));
            let local = pair
                .and_then(|p| p["localCandidateId"].as_str())
                .and_then(|id| stats.iter().find(|s| s["id"] == id));
            let remote = pair
                .and_then(|p| p["remoteCandidateId"].as_str())
                .and_then(|id| stats.iter().find(|s| s["id"] == id));
            reports.push(serde_json::json!({"session":session,"pair":pair,
                "local":local,"remote":remote}));
        }
        Ok(serde_json::json!(reports))
    }

    pub(super) fn new() -> Self {
        Self {
            sessions: Rc::new(RefCell::new(HashMap::new())),
        }
    }
}

impl Clone for BrowserRtcRegistry {
    fn clone(&self) -> Self {
        Self {
            sessions: self.sessions.clone(),
        }
    }
}

impl BrowserRtcRegistry {
    pub(crate) fn create_peer_connection(
        &self,
        session_key: String,
        _role: BrowserRtcSessionRole,
        _generation: u64,
        stun_urls: Option<Vec<String>>,
        _remote_endpoint: String,
    ) -> Result<()> {
        validate_rtc_peer_connection_available()?;
        validate_session_key(&session_key)?;
        if self.sessions.borrow().contains_key(&session_key) {
            return Err(Error::InvalidConfig("duplicate WebRTC session key"));
        }

        let ice_config = ice_config_from_js(stun_urls)?;
        let configuration = RtcConfiguration::new();
        apply_ice_config(&configuration, &ice_config)?;
        let peer = RtcPeerConnection::new_with_configuration(&configuration).map_err(js_error)?;
        let local_ice =
            LocalIceQueue::with_capacity(crate::config::DEFAULT_LOCAL_ICE_QUEUE_CAPACITY);
        let incoming_data_channels = Rc::new(RefCell::new(VecDeque::new()));
        let incoming_data_channel_waiters = Rc::new(RefCell::new(VecDeque::<
            oneshot::Sender<RtcDataChannel>,
        >::new()));

        let ice_queue = local_ice.clone();
        let ice_handler = Closure::wrap(Box::new(move |event: RtcPeerConnectionIceEvent| {
            let local_event = match event.candidate() {
                Some(candidate) => LocalIceEvent::Candidate(WebRtcIceCandidate {
                    candidate: candidate.candidate(),
                    sdp_mid: candidate.sdp_mid(),
                    sdp_mline_index: candidate.sdp_m_line_index(),
                }),
                None => LocalIceEvent::EndOfCandidates,
            };
            let _ = ice_queue.push(local_event);
        }) as Box<dyn FnMut(_)>);
        peer.set_onicecandidate(Some(ice_handler.as_ref().unchecked_ref()));

        let incoming_queue = incoming_data_channels.clone();
        let incoming_waiters = incoming_data_channel_waiters.clone();
        let data_channel_handler = Closure::wrap(Box::new(move |event: RtcDataChannelEvent| {
            let channel = event.channel();
            channel.set_binary_type(RtcDataChannelType::Arraybuffer);
            match validate_data_channel_transferable(&channel) {
                Ok(()) => {
                    let waiter = incoming_waiters.borrow_mut().pop_front();
                    if let Some(waiter) = waiter {
                        let _ = waiter.send(channel);
                    } else {
                        incoming_queue.borrow_mut().push_back(channel);
                    }
                }
                Err(_) => {}
            }
        }) as Box<dyn FnMut(_)>);
        peer.set_ondatachannel(Some(data_channel_handler.as_ref().unchecked_ref()));

        self.sessions.borrow_mut().insert(
            session_key.clone(),
            BrowserRtcSession {
                peer,
                local_ice,
                incoming_data_channels,
                incoming_data_channel_waiters,
                _ice_handler: ice_handler,
                _data_channel_handler: data_channel_handler,
            },
        );
        Ok(())
    }

    pub(crate) fn create_data_channel(
        &self,
        session_key: &str,
        label: Option<&str>,
        protocol: Option<&str>,
        ordered: bool,
        max_retransmits: u16,
    ) -> Result<RtcDataChannel> {
        if ordered {
            return Err(Error::InvalidConfig(
                "browser WebRTC data channels must be unordered",
            ));
        }
        if max_retransmits != 0 {
            return Err(Error::InvalidConfig(
                "browser WebRTC data channels must use maxRetransmits 0",
            ));
        }
        let peer = self.peer_for(session_key)?;
        let init = RtcDataChannelInit::new();
        init.set_ordered(ordered);
        init.set_max_retransmits(max_retransmits);
        if let Some(protocol) = protocol.filter(|protocol| !protocol.is_empty()) {
            init.set_protocol(protocol);
        }
        let label = label
            .filter(|label| !label.is_empty())
            .unwrap_or(WEBRTC_DATA_CHANNEL_LABEL);
        let channel = peer.create_data_channel_with_data_channel_dict(label, &init);
        channel.set_binary_type(RtcDataChannelType::Arraybuffer);
        validate_data_channel_transferable(&channel)?;
        Ok(channel)
    }

    pub(crate) async fn take_data_channel(&self, session_key: &str) -> Result<RtcDataChannel> {
        self.wait_next_data_channel(session_key).await
    }

    pub(crate) async fn create_offer(&self, session_key: &str) -> Result<String> {
        let peer = self.peer_for(session_key)?;
        create_offer_for_peer(&peer).await
    }

    pub(crate) async fn accept_offer(
        &self,
        session_key: &str,
        _remote_endpoint: Option<String>,
        sdp: &str,
    ) -> Result<()> {
        let peer = self.peer_for(session_key)?;
        set_remote_description_for_peer(&peer, RtcSdpType::Offer, sdp).await
    }

    pub(crate) async fn create_answer(&self, session_key: &str) -> Result<String> {
        let peer = self.peer_for(session_key)?;
        create_answer_for_peer(&peer).await
    }

    pub(crate) async fn accept_answer(&self, session_key: &str, sdp: &str) -> Result<()> {
        let peer = self.peer_for(session_key)?;
        set_remote_description_for_peer(&peer, RtcSdpType::Answer, sdp).await
    }

    pub(crate) async fn next_local_ice(&self, session_key: &str) -> Result<BrowserIceCandidate> {
        let local_ice = self.local_ice_for(session_key)?;
        local_ice.next().await
    }

    pub(crate) async fn add_remote_ice(
        &self,
        session_key: &str,
        ice: BrowserIceCandidate,
    ) -> Result<()> {
        let peer = self.peer_for(session_key)?;
        match ice {
            LocalIceEvent::Candidate(candidate) => {
                add_ice_candidate_for_peer(&peer, &candidate).await?;
            }
            LocalIceEvent::EndOfCandidates => {
                add_end_of_candidates_for_peer(&peer).await?;
            }
        }
        Ok(())
    }

    pub(crate) fn close_peer_connection(
        &self,
        session_key: &str,
        _reason: Option<String>,
    ) -> Result<()> {
        self.try_close_peer_connection(session_key)
    }

    fn try_take_next_data_channel(&self, session_key: &str) -> Result<Option<RtcDataChannel>> {
        let incoming = self.incoming_data_channels_for(session_key)?;
        let channel = incoming.borrow_mut().pop_front();
        if let Some(channel) = &channel {
            validate_data_channel_transferable(channel)?;
        }
        Ok(channel)
    }

    async fn wait_next_data_channel(&self, session_key: &str) -> Result<RtcDataChannel> {
        if let Some(channel) = self.try_take_next_data_channel(session_key)? {
            return Ok(channel);
        }
        let waiters = self.incoming_data_channel_waiters_for(session_key)?;
        let (sender, receiver) = oneshot::channel();
        waiters.borrow_mut().push_back(sender);
        receiver.await.map_err(|_| Error::SessionClosed)
    }

    fn try_close_peer_connection(&self, session_key: &str) -> Result<()> {
        validate_session_key(session_key)?;
        if let Some(session) = self.sessions.borrow_mut().remove(session_key) {
            session.peer.close();
            session.local_ice.close();
        }
        Ok(())
    }

    fn peer_for(&self, session_key: &str) -> Result<RtcPeerConnection> {
        validate_session_key(session_key)?;
        self.sessions
            .borrow()
            .get(session_key)
            .map(|session| session.peer.clone())
            .ok_or(Error::UnknownSession)
    }

    fn local_ice_for(&self, session_key: &str) -> Result<LocalIceQueue> {
        validate_session_key(session_key)?;
        self.sessions
            .borrow()
            .get(session_key)
            .map(|session| session.local_ice.clone())
            .ok_or(Error::UnknownSession)
    }

    fn incoming_data_channels_for(
        &self,
        session_key: &str,
    ) -> Result<Rc<RefCell<VecDeque<RtcDataChannel>>>> {
        validate_session_key(session_key)?;
        self.sessions
            .borrow()
            .get(session_key)
            .map(|session| session.incoming_data_channels.clone())
            .ok_or(Error::UnknownSession)
    }

    fn incoming_data_channel_waiters_for(
        &self,
        session_key: &str,
    ) -> Result<Rc<RefCell<VecDeque<oneshot::Sender<RtcDataChannel>>>>> {
        validate_session_key(session_key)?;
        self.sessions
            .borrow()
            .get(session_key)
            .map(|session| session.incoming_data_channel_waiters.clone())
            .ok_or(Error::UnknownSession)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BrowserRtcSessionRole {
    Dialer,
    Acceptor,
}

pub(crate) type BrowserIceCandidate = LocalIceEvent;
