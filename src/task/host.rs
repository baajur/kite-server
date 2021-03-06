use super::model::{AgentInfo, AgentInfoRequest};
use super::protocol::{Request, RequestPayload, Response, ResponsePayload};
use super::{Agent, AgentManager, AgentStatus, HostError, RequestQueue};
use crate::config::CONFIG;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;
use tokio::io::{AsyncWriteExt, BufReader, BufWriter};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, mpsc, oneshot, Mutex, RwLock};
use tokio::time::Duration;

use super::Result;
use crate::task::HaltChannel;
use log::{error, info, warn};

impl Clone for HaltChannel {
    fn clone(&self) -> Self {
        Self {
            sender: self.sender.clone(),
            receiver: self.sender.subscribe(),
        }
    }
}

impl Agent {
    /// An agent instance.
    pub fn new(basic: AgentInfo, addr: SocketAddr) -> Self {
        Self {
            basic,
            addr,
            queue: Default::default(),
            channel: None,
            halt: None,
        }
    }

    /// Send request to the agent
    async fn send(&mut self, request: Request) -> Result<()> {
        let mut channel = self.channel.clone().ok_or(HostError::AgentUnavailable)?;

        // Send request packet to the sender loop to post, if the process failed, set channel to None
        // so that the next try could return immediately.
        channel.send(request).await.or_else(move |_| {
            self.channel = None;
            Err(HostError::AgentUnavailable)
        });
        Ok(())
    }

    /// Request to agent, and add an oneshot sender and it can be used when the response received.
    /// Return HostError timeout if the agent doesn't respond in a reasonal time.
    pub async fn request(&mut self, request: RequestPayload) -> Result<Response> {
        let request = Request::new(request);
        let seq = request.seq;

        // Send the request to the sender loop
        self.send(request).await?;

        // Result channel, return rx to the caller and save the tx to the queue.
        let (tx, rx) = oneshot::channel();
        // Use a pair of big parentheses, to drop queue automatically.
        {
            let mut queue = self.queue.lock().await;
            queue.insert(seq, tx);
        }
        match tokio::time::timeout(Duration::from_millis(5000), rx).await {
            Ok(result) => Ok(result?),
            Err(_) => {
                let mut queue = self.queue.lock().await;
                queue.remove(&seq);

                Err(HostError::Timeout.into())
            }
        }
    }

    /// Select requester and post the response.
    async fn dispatch_response(queue: Arc<Mutex<RequestQueue>>, response: Response) {
        let mut queue = queue.lock().await;

        if let Some(sender) = queue.remove(&response.ack) {
            sender.send(response);
        } else {
            warn!("Received a response message without corresponding request.");
        }
    }

    /// Sender loop: send requests to agent over ws.
    async fn sender_loop(
        socket_tx: OwnedWriteHalf,
        mut request_rx: mpsc::Receiver<Request>,
        mut halt: HaltChannel,
    ) -> Result<()> {
        info!("Sender loop started");
        let mut buffer = BufWriter::new(socket_tx);

        loop {
            tokio::select! {
                Some(request) = request_rx.recv() => {
                    buffer.write_u64(request.seq).await?;
                    buffer.write_u32(request.size).await?;
                    buffer.write_all(&request.payload).await?;
                    buffer.flush().await?;

                    info!("Send packet");
                }
                _ = halt.receiver.recv() => {
                    break;
                }
            }
        }
        info!("Sender loop exited.");
        Ok(())
    }

    /// Receiver loop: receive responses from the agent and transfer it to the requester.
    async fn receiver_loop(
        socket_rx: OwnedReadHalf,
        queue: Arc<Mutex<RequestQueue>>,
        mut halt: HaltChannel,
    ) -> Result<()> {
        info!("Receiver loop started");
        let mut buffer = BufReader::new(socket_rx);

        loop {
            tokio::select! {
                result = Response::from_stream(&mut buffer) => {
                    match result {
                        Ok(response) => {
                            info!("Packet received: {:?}", response);
                            Self::dispatch_response(queue.clone(), response).await;
                        }
                        Err(e) => {
                            warn!("Connection lost: {:?}", e);
                            halt.sender.send(());
                            break;
                        }
                    }
                }
                _ = halt.receiver.recv() => {
                    break;
                }
            }
        }
        info!("Receiver loop exited.");
        Ok(())
    }

    async fn heartbeat_loop(last_update: Arc<RwLock<Instant>>, mut tx: mpsc::Sender<Request>) {
        let timeout = Duration::from_secs(20);
        let mut f = false;

        loop {
            let elapsed = { last_update.read().await.elapsed().as_secs() };
            if elapsed > 30 {
                if !f {
                    tx.send(Request::default()).await;
                    f = true;
                } else {
                    break;
                }
            } else {
                f = false;
            }
            tokio::time::delay_for(timeout).await;
        }
        info!("Heartbeat loop exited.");
    }

    /// Start listening response from the agent and request from web.
    pub async fn start(&mut self, stream: TcpStream) {
        let (recv_half, send_half) = stream.into_split();
        let (tx, rx) = mpsc::channel(128);
        let (halt_tx, halt_rx) = broadcast::channel(1);

        tokio::spawn(Self::receiver_loop(
            recv_half,
            self.queue.clone(),
            HaltChannel {
                sender: halt_tx.clone(),
                receiver: halt_tx.subscribe(),
            },
        ));
        tokio::spawn(Self::sender_loop(
            send_half,
            rx,
            HaltChannel {
                sender: halt_tx.clone(),
                receiver: halt_tx.subscribe(),
            },
        ));
        // tokio::spawn(Self::heartbeat_loop(self.last_update.clone(), tx.clone()));
        self.channel = Some(tx);
        self.halt = Some(HaltChannel {
            sender: halt_tx,
            receiver: halt_rx,
        });
    }

    pub fn available(&self) -> bool {
        self.halt.is_some()
    }

    pub async fn join(&mut self) {
        if let Some(channel) = self.halt.clone() {
            let mut rx = channel.receiver;

            rx.recv().await;
            tokio::time::delay_for(Duration::from_secs(1)).await;
            self.halt = None;

            return;
        }
    }

    /// Halt the agent client
    pub fn stop(&mut self) {
        if let Some(halt_channel) = self.halt.take() {
            halt_channel.sender.send(());
        }
    }
}

impl AgentManager {
    /// Create a new host instance.
    pub fn new() -> Self {
        info!("A Host instance created.");
        Self {
            agents: Arc::new(Default::default()),
        }
    }

    /// Select an agent randomly and send request packet.
    pub async fn request(&self, request: RequestPayload) -> Result<Response> {
        use rand::prelude::IteratorRandom;

        let mut rng = rand::thread_rng();
        let mut agents = self.agents.lock().await;

        let agent = agents.iter_mut().choose(&mut rng);
        // Send to an agent and record this request.
        if let Some((_, agent)) = agent {
            agent.request(request).await
        } else {
            Err(HostError::NoAgentAvailable.into())
        }
    }

    /// Get agent list
    pub async fn get_agent_list(&self) -> Vec<AgentStatus> {
        let agents = self.agents.lock().await;

        agents
            .iter()
            .map(|(_, agent)| AgentStatus {
                name: agent.basic.name.clone(),
                intranet_addr: "".to_string(),
                external_addr: agent.addr.to_string(),
                queue: 0u16,
            })
            .collect()
    }

    async fn start(&self, stream: TcpStream, peer: SocketAddr) -> Result<()> {
        info!("New agent connection established: {}", peer.to_string());
        let mut agent = Agent::new(AgentInfo { name: "".to_string() }, peer);

        agent.start(stream).await;
        let response = agent
            .request(RequestPayload::AgentInfo(AgentInfoRequest))
            .await
            .map_err(|_| HostError::AgentUnavailable)?;
        if let ResponsePayload::AgentInfo(base_info) = response.payload()?? {
            agent.basic = base_info;
            {
                let mut agents = self.agents.lock().await;
                agents.insert(peer, agent);
            }
            Ok(())
        } else {
            Err(HostError::AgentUnavailable.into())
        }
    }

    pub async fn wait(self, peer: SocketAddr) {
        let agent = {
            let agents = self.agents.lock().await;
            agents.get(&peer).map(Clone::clone)
        };

        match agent {
            Some(mut agent) => {
                // Wait for agent breaks
                agent.join().await;
                // Clear agent in agent list and return
                let mut agents = self.agents.lock().await;
                agents.remove(&peer);
            }
            None => (),
        }
    }

    pub async fn agent_main(&self) -> Result<()> {
        let mut listener = TcpListener::bind(&CONFIG.host.bind).await?;

        while let Ok((stream, peer)) = listener.accept().await {
            info!("New agent connection established, with {}", peer);

            let new_handler = self.clone();
            tokio::spawn(async move {
                let addr = peer;
                match new_handler.start(stream, addr.clone()).await {
                    Ok(_) => {
                        info!("One agent task started.");
                        new_handler.wait(addr).await;
                        info!("Agent exited.");
                    }
                    Err(e) => error!("One agent task started failed with {:?}", e),
                }
            });
        }
        Ok(())
    }
}
