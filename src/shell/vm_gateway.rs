//! A transparent Tor gateway for one VM's third network card.
//!
//! In Tor mode a VM's only way out is this gateway: the card's QEMU backend is
//! a unix socket (`-netdev stream`, see `vm::qemu_args`) that this module
//! serves with a small userspace network stack ([`smoltcp`]), and everything
//! the guest sends is handled here and nowhere else:
//!
//! * DHCP: the guest gets [`GUEST_IP`] with [`GATEWAY_IP`] as router and DNS;
//! * DNS (UDP to any address): every name is answered with a made-up address
//!   from [`FAKE_NET`] and remembered, so the guest's connection to it can be
//!   handed to Tor *by name* (the exit resolves it: no DNS leaves the host,
//!   and `.onion` names work). IPv6 and other record types get empty answers;
//! * TCP (to any address): terminated here, then relayed through the host's
//!   `tor` over SOCKS5, to the name behind a made-up address or to the plain
//!   address otherwise. Each VM logs in to Tor with its own SOCKS credentials,
//!   which Tor's stream isolation turns into its own circuits;
//! * everything else (other UDP, ICMP, ...) is dropped.
//!
//! The guest cannot get around it: this is the card's whole network. What
//! decides *whether* the card is connected is the link state set over QMP (see
//! `vm_state::set_network_over_socket`); the gateway itself just serves
//! whatever arrives.
//!
//! [`Gateway::new_identity`] is the "new circuit" button: it changes the SOCKS
//! credentials for new connections and resets the guest's open ones, so
//! nothing the guest did before stays linked to what it does next.
//!
//! One thread per gateway, started by [`Gateway::spawn`] before QEMU is (QEMU
//! connects at startup). The stack is single-threaded; the SOCKS handshakes
//! are non-blocking state machines so a slow Tor stalls only its own flows.

use std::{
    collections::{HashMap, VecDeque},
    io::{self, Read, Write},
    net::{Ipv4Addr, Shutdown, SocketAddr, TcpStream},
    os::{
        fd::{AsFd, BorrowedFd},
        unix::net::{UnixListener, UnixStream},
    },
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU16, AtomicU32, AtomicU64, Ordering},
    },
    time::{Duration, Instant as StdInstant},
};

use rustix::event::{PollFd, PollFlags};
use smoltcp::{
    iface::{Config, Interface, SocketHandle, SocketSet},
    phy::{ChecksumCapabilities, Device, DeviceCapabilities, Medium, PacketMeta, RxToken, TxToken},
    socket::{tcp, udp},
    time::Instant,
    wire::{
        ArpPacket, ArpRepr, DhcpMessageType, DhcpPacket, DhcpRepr, EthernetAddress, EthernetFrame,
        EthernetProtocol, EthernetRepr, HardwareAddress, IpAddress, IpCidr, IpEndpoint, IpProtocol,
        Ipv4Packet, Ipv4Repr, TcpControl, TcpPacket, TcpRepr, UdpPacket, UdpRepr,
    },
};
use tracing::{debug, warn};

/// The guest's address on the Tor card, and its router and DNS server.
pub const GUEST_IP: Ipv4Addr = Ipv4Addr::new(10, 0, 4, 15);
pub const GATEWAY_IP: Ipv4Addr = Ipv4Addr::new(10, 0, 4, 2);
const NETMASK: Ipv4Addr = Ipv4Addr::new(255, 255, 255, 0);
const GATEWAY_MAC: [u8; 6] = [0x02, 0x49, 0x52, 0x4c, 0x44, 0x01];

/// Addresses handed out for names (10.192.0.0/10): a connection to one of
/// them is a connection to the name it was given for.
const FAKE_BASE: u32 = 0x0AC0_0000;
const FAKE_COUNT: u32 = 1 << 22;
/// Names remembered at most (older ones lose their address when it is reused).
const FAKE_NAMES: u32 = 1 << 14;

const FLOW_BUFFER: usize = 64 * 1024;
const CHUNK: usize = 16 * 1024;
const MAX_FLOWS: usize = 256;
const MAX_LISTEN_PORTS: usize = 64;
/// How long Tor gets to build a circuit and connect, from the guest's SYN.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_FRAME: usize = 65_536;

// ---------------------------------------------------------------------------
// Shared state
// ---------------------------------------------------------------------------

/// Where the host's `tor` listens (0: it isn't running). Shared by every
/// gateway; the Tor service updates it.
#[derive(Debug, Clone, Default)]
pub struct TorEndpoint(Arc<AtomicU16>);

impl TorEndpoint {
    pub fn set(&self, port: Option<u16>) {
        self.0.store(port.unwrap_or(0), Ordering::SeqCst);
    }

    pub fn get(&self) -> Option<u16> {
        Some(self.0.load(Ordering::SeqCst)).filter(|&p| p != 0)
    }
}

/// What the overview shows about a gateway.
#[derive(Debug, Default)]
pub struct GatewayStats {
    /// Connections the guest has open through Tor.
    pub flows: AtomicU32,
    /// Starts at 1; each "new identity" adds one.
    pub identity: AtomicU32,
    pub bytes_up: AtomicU64,
    pub bytes_down: AtomicU64,
}

/// Handle to a running gateway. Dropping it stops the thread.
#[derive(Debug)]
pub struct Gateway {
    stats: Arc<GatewayStats>,
    cancel: Arc<AtomicBool>,
}

impl Gateway {
    /// Starts serving the unix socket at `path` (created here, so QEMU can
    /// connect to it as soon as it starts). `id` names the VM in the SOCKS
    /// credentials and the thread.
    pub fn spawn(id: u64, path: &Path, tor: TorEndpoint) -> io::Result<Self> {
        let listener = UnixListener::bind(path)?;
        listener.set_nonblocking(true)?;
        let stats = Arc::new(GatewayStats {
            identity: AtomicU32::new(1),
            ..GatewayStats::default()
        });
        let cancel = Arc::new(AtomicBool::new(false));
        let mut state = Serve::new(id, listener, tor, stats.clone(), cancel.clone());
        std::thread::Builder::new()
            .name(format!("vm-tor-{id}"))
            .spawn(move || state.run())?;
        Ok(Self { stats, cancel })
    }

    pub fn stats(&self) -> &GatewayStats {
        &self.stats
    }

    /// Switches to fresh Tor circuits: new connections log in to Tor under
    /// new credentials, and the guest's open connections are reset within a
    /// moment.
    pub fn new_identity(&self) {
        self.stats.identity.fetch_add(1, Ordering::SeqCst);
    }
}

impl Drop for Gateway {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::SeqCst);
    }
}

// ---------------------------------------------------------------------------
// Frames on the unix socket: a 4-byte big-endian length, then the Ethernet
// frame (QEMU's `stream` netdev format).
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct FrameReader {
    buf: Vec<u8>,
}

impl FrameReader {
    fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// The next complete frame; `Err` if the stream is not in step (a length
    /// no frame has).
    fn next_frame(&mut self) -> Result<Option<Vec<u8>>, ()> {
        let Some(len) = self.buf.get(..4) else {
            return Ok(None);
        };
        let len = u32::from_be_bytes([len[0], len[1], len[2], len[3]]) as usize;
        if len == 0 || len > MAX_FRAME {
            return Err(());
        }
        if self.buf.len() < 4 + len {
            return Ok(None);
        }
        let frame = self.buf[4..4 + len].to_vec();
        self.buf.drain(..4 + len);
        Ok(Some(frame))
    }
}

fn frame_for_stream(frame: &[u8], out: &mut VecDeque<u8>) {
    out.extend((frame.len() as u32).to_be_bytes());
    out.extend(frame);
}

// ---------------------------------------------------------------------------
// The network device smoltcp runs on: two queues of frames.
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct QueueDevice {
    rx: VecDeque<Vec<u8>>,
    tx: VecDeque<Vec<u8>>,
}

struct QueueRx(Vec<u8>);
struct QueueTx<'a>(&'a mut VecDeque<Vec<u8>>);

impl RxToken for QueueRx {
    fn consume<R, F: FnOnce(&[u8]) -> R>(self, f: F) -> R {
        f(&self.0)
    }
}

impl TxToken for QueueTx<'_> {
    fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, len: usize, f: F) -> R {
        let mut buffer = vec![0; len];
        let result = f(&mut buffer);
        self.0.push_back(buffer);
        result
    }
}

impl Device for QueueDevice {
    type RxToken<'a> = QueueRx;
    type TxToken<'a> = QueueTx<'a>;

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.medium = Medium::Ethernet;
        caps.max_transmission_unit = 1514;
        caps
    }

    fn receive(&mut self, _: Instant) -> Option<(QueueRx, QueueTx<'_>)> {
        let frame = self.rx.pop_front()?;
        Some((QueueRx(frame), QueueTx(&mut self.tx)))
    }

    fn transmit(&mut self, _: Instant) -> Option<QueueTx<'_>> {
        Some(QueueTx(&mut self.tx))
    }
}

// ---------------------------------------------------------------------------
// Looking at incoming frames
// ---------------------------------------------------------------------------

#[derive(Debug, PartialEq, Eq)]
enum Ingress {
    /// A DHCP client message; the payload of the UDP datagram.
    Dhcp(Vec<u8>),
    /// A connection attempt to this address and port: it may be refused at
    /// once, and otherwise a listener is needed first.
    TcpSyn { dst: Ipv4Addr, port: u16 },
    /// For the stack to process.
    Pass,
    /// Not for anyone here: dropped without a look from the stack.
    Drop,
}

/// What a frame from the guest is for. The stack gets only what the gateway
/// is meant to serve - ARP for its own address, TCP, DNS - and nothing else:
/// in particular, because the stack accepts connections to *every* address,
/// it would also answer ARP for every address (including the guest's own,
/// which a DHCP client's duplicate-address probe then takes as a conflict)
/// and answer pings to anywhere.
fn classify(frame: &[u8]) -> Ingress {
    let Ok(eth) = EthernetFrame::new_checked(frame) else {
        return Ingress::Drop;
    };
    match eth.ethertype() {
        EthernetProtocol::Arp => {
            let Ok(arp) = ArpPacket::new_checked(eth.payload()) else {
                return Ingress::Drop;
            };
            match ArpRepr::parse(&arp) {
                Ok(ArpRepr::EthernetIpv4 {
                    target_protocol_addr,
                    ..
                }) if target_protocol_addr == GATEWAY_IP => Ingress::Pass,
                _ => Ingress::Drop,
            }
        }
        EthernetProtocol::Ipv4 => {
            let Ok(ip) = Ipv4Packet::new_checked(eth.payload()) else {
                return Ingress::Drop;
            };
            match ip.next_header() {
                IpProtocol::Udp => match UdpPacket::new_checked(ip.payload()) {
                    Ok(udp) if udp.dst_port() == 67 => Ingress::Dhcp(udp.payload().to_vec()),
                    Ok(udp) if udp.dst_port() == 53 => Ingress::Pass,
                    _ => Ingress::Drop,
                },
                IpProtocol::Tcp => match TcpPacket::new_checked(ip.payload()) {
                    Ok(tcp) if tcp.syn() && !tcp.ack() => Ingress::TcpSyn {
                        dst: ip.dst_addr(),
                        port: tcp.dst_port(),
                    },
                    Ok(_) => Ingress::Pass,
                    Err(_) => Ingress::Drop,
                },
                _ => Ingress::Drop,
            }
        }
        _ => Ingress::Drop,
    }
}

/// A reset in answer to the TCP SYN in `frame`: what a closed port says.
fn tcp_reset(frame: &[u8]) -> Option<Vec<u8>> {
    let eth = EthernetFrame::new_checked(frame).ok()?;
    let ip = Ipv4Packet::new_checked(eth.payload()).ok()?;
    let syn = TcpPacket::new_checked(ip.payload()).ok()?;
    let tcp = TcpRepr {
        src_port: syn.dst_port(),
        dst_port: syn.src_port(),
        control: TcpControl::Rst,
        seq_number: smoltcp::wire::TcpSeqNumber(0),
        ack_number: Some(syn.seq_number() + 1),
        window_len: 0,
        window_scale: None,
        max_seg_size: None,
        sack_permitted: false,
        sack_ranges: [None; 3],
        timestamp: None,
        payload: &[],
    };
    let caps = ChecksumCapabilities::default();
    let eth_out = EthernetRepr {
        src_addr: EthernetAddress(GATEWAY_MAC),
        dst_addr: eth.src_addr(),
        ethertype: EthernetProtocol::Ipv4,
    };
    let ip_out = Ipv4Repr {
        src_addr: ip.dst_addr(),
        dst_addr: ip.src_addr(),
        next_header: IpProtocol::Tcp,
        payload_len: tcp.buffer_len(),
        hop_limit: 64,
    };
    let mut out = vec![0u8; eth_out.buffer_len() + ip_out.buffer_len() + tcp.buffer_len()];
    eth_out.emit(&mut EthernetFrame::new_unchecked(&mut out[..]));
    let ip_at = eth_out.buffer_len();
    ip_out.emit(&mut Ipv4Packet::new_unchecked(&mut out[ip_at..]), &caps);
    let tcp_at = ip_at + ip_out.buffer_len();
    tcp.emit(
        &mut TcpPacket::new_unchecked(&mut out[tcp_at..]),
        &IpAddress::Ipv4(ip_out.src_addr),
        &IpAddress::Ipv4(ip_out.dst_addr),
        &caps,
    );
    Some(out)
}

// ---------------------------------------------------------------------------
// DHCP
// ---------------------------------------------------------------------------

/// A complete Ethernet frame carrying a UDP datagram.
fn udp_frame(
    src: (Ipv4Addr, u16),
    dst: (Ipv4Addr, u16),
    dst_mac: EthernetAddress,
    payload: &[u8],
) -> Vec<u8> {
    let caps = ChecksumCapabilities::default();
    let eth = EthernetRepr {
        src_addr: EthernetAddress(GATEWAY_MAC),
        dst_addr: dst_mac,
        ethertype: EthernetProtocol::Ipv4,
    };
    let udp = UdpRepr {
        src_port: src.1,
        dst_port: dst.1,
    };
    let ip = Ipv4Repr {
        src_addr: src.0,
        dst_addr: dst.0,
        next_header: IpProtocol::Udp,
        payload_len: 8 + payload.len(),
        hop_limit: 64,
    };
    let mut frame = vec![0u8; eth.buffer_len() + ip.buffer_len() + 8 + payload.len()];
    eth.emit(&mut EthernetFrame::new_unchecked(&mut frame[..]));
    let ip_at = eth.buffer_len();
    ip.emit(&mut Ipv4Packet::new_unchecked(&mut frame[ip_at..]), &caps);
    let udp_at = ip_at + ip.buffer_len();
    udp.emit(
        &mut UdpPacket::new_unchecked(&mut frame[udp_at..]),
        &IpAddress::Ipv4(src.0),
        &IpAddress::Ipv4(dst.0),
        payload.len(),
        |out| out.copy_from_slice(payload),
        &caps,
    );
    frame
}

/// The server's answer to a DHCP message, as a frame to send, if it deserves
/// one. The guest has one address, so there is nothing to allocate: DISCOVER
/// is offered it, REQUEST gets it (or a refusal if it asks for another).
fn dhcp_reply(payload: &[u8]) -> Option<Vec<u8>> {
    let packet = DhcpPacket::new_checked(payload).ok()?;
    let request = DhcpRepr::parse(&packet).ok()?;
    let reply_type = match request.message_type {
        DhcpMessageType::Discover => DhcpMessageType::Offer,
        DhcpMessageType::Request => {
            let wanted = request
                .requested_ip
                .or(Some(request.client_ip).filter(|a| !a.is_unspecified()));
            if wanted.is_none_or(|ip| ip == GUEST_IP) {
                DhcpMessageType::Ack
            } else {
                DhcpMessageType::Nak
            }
        }
        _ => return None,
    };
    let nak = reply_type == DhcpMessageType::Nak;
    let dns = heapless::Vec::from_slice(&[GATEWAY_IP]).ok()?;
    let reply = DhcpRepr {
        message_type: reply_type,
        transaction_id: request.transaction_id,
        secs: 0,
        client_hardware_address: request.client_hardware_address,
        client_ip: Ipv4Addr::UNSPECIFIED,
        your_ip: if nak { Ipv4Addr::UNSPECIFIED } else { GUEST_IP },
        server_ip: GATEWAY_IP,
        router: (!nak).then_some(GATEWAY_IP),
        subnet_mask: (!nak).then_some(NETMASK),
        relay_agent_ip: Ipv4Addr::UNSPECIFIED,
        broadcast: true,
        requested_ip: None,
        client_identifier: None,
        server_identifier: Some(GATEWAY_IP),
        parameter_request_list: None,
        dns_servers: (!nak).then_some(dns),
        max_size: None,
        lease_duration: (!nak).then_some(86_400),
        renew_duration: None,
        rebind_duration: None,
        additional_options: &[],
    };
    let mut bytes = vec![0u8; reply.buffer_len()];
    reply
        .emit(&mut DhcpPacket::new_unchecked(&mut bytes[..]))
        .ok()?;
    // Always broadcast: the client may not have an address to receive on yet.
    Some(udp_frame(
        (GATEWAY_IP, 67),
        (Ipv4Addr::BROADCAST, 68),
        EthernetAddress::BROADCAST,
        &bytes,
    ))
}

// ---------------------------------------------------------------------------
// DNS
// ---------------------------------------------------------------------------

/// The names the guest asked for and the made-up addresses given for them.
#[derive(Debug, Default)]
struct FakeDns {
    next: u32,
    by_name: HashMap<String, Ipv4Addr>,
    by_addr: HashMap<Ipv4Addr, String>,
}

impl FakeDns {
    fn address_for(&mut self, name: &str) -> Ipv4Addr {
        let name = name.to_ascii_lowercase();
        if let Some(&ip) = self.by_name.get(&name) {
            return ip;
        }
        // Reusing an address forgets the name it had.
        let ip = Ipv4Addr::from(FAKE_BASE + 1 + self.next % FAKE_NAMES.min(FAKE_COUNT - 2));
        self.next = self.next.wrapping_add(1);
        if let Some(old) = self.by_addr.insert(ip, name.clone()) {
            self.by_name.remove(&old);
        }
        self.by_name.insert(name, ip);
        ip
    }

    fn name_of(&self, ip: Ipv4Addr) -> Option<&str> {
        self.by_addr.get(&ip).map(String::as_str)
    }
}

fn is_fake(ip: Ipv4Addr) -> bool {
    let n = u32::from(ip);
    (FAKE_BASE..FAKE_BASE + FAKE_COUNT).contains(&n)
}

const QTYPE_A: u16 = 1;
const QTYPE_AAAA: u16 = 28;
const QTYPE_PTR: u16 = 12;

/// Parses the single question of a DNS query: `(name, qtype, end of question)`.
fn parse_question(query: &[u8]) -> Option<(String, u16, usize)> {
    if query.len() < 12 || query[2] & 0x80 != 0 {
        return None; // too short, or a response
    }
    if u16::from_be_bytes([query[4], query[5]]) != 1 {
        return None;
    }
    let mut at = 12;
    let mut name = String::new();
    loop {
        let len = *query.get(at)? as usize;
        at += 1;
        if len == 0 {
            break;
        }
        if len > 63 {
            return None; // compression is not valid in a question
        }
        let label = query.get(at..at + len)?;
        if !name.is_empty() {
            name.push('.');
        }
        name.push_str(std::str::from_utf8(label).ok()?);
        at += len;
    }
    let qtype = u16::from_be_bytes([*query.get(at)?, *query.get(at + 1)?]);
    let qclass = u16::from_be_bytes([*query.get(at + 2)?, *query.get(at + 3)?]);
    (qclass == 1 && !name.is_empty()).then_some((name, qtype, at + 4))
}

/// The answer to a DNS query, or `None` for what isn't one. Names get fake
/// addresses (A), IPv6 and other kinds are "no such record" so resolvers move
/// on, and reverse lookups say the name doesn't exist.
fn dns_answer(query: &[u8], fake: &mut FakeDns) -> Option<Vec<u8>> {
    let (name, qtype, question_end) = parse_question(query)?;
    let mut out = Vec::with_capacity(question_end + 16);
    out.extend_from_slice(&query[..2]); // id
    let recursion_desired = query[2] & 1;
    let rcode = if qtype == QTYPE_PTR { 3 } else { 0 };
    out.push(0x80 | recursion_desired); // response, standard query
    out.push(0x80 | rcode); // recursion available
    out.extend_from_slice(&[0, 1]); // one question
    let answers: u16 = (qtype == QTYPE_A) as u16;
    out.extend_from_slice(&answers.to_be_bytes());
    out.extend_from_slice(&[0, 0, 0, 0]);
    out.extend_from_slice(&query[12..question_end]);
    if qtype == QTYPE_A {
        let ip = fake.address_for(&name);
        out.extend_from_slice(&[0xC0, 0x0C]); // the question's name
        out.extend_from_slice(&QTYPE_A.to_be_bytes());
        out.extend_from_slice(&[0, 1]); // class IN
        out.extend_from_slice(&30u32.to_be_bytes()); // TTL
        out.extend_from_slice(&[0, 4]);
        out.extend_from_slice(&ip.octets());
    } else {
        let _ = QTYPE_AAAA;
    }
    Some(out)
}

// ---------------------------------------------------------------------------
// SOCKS5 towards Tor
// ---------------------------------------------------------------------------

/// Where a guest connection goes.
#[derive(Debug, PartialEq, Eq)]
enum Target {
    Host(String),
    Addr(Ipv4Addr),
}

/// Decides what a connection to `dst` becomes: the name behind a made-up
/// address, a plain public address, or nothing (made-up addresses that were
/// never handed out, and addresses that could only mean "this network").
fn target_for(dst: Ipv4Addr, fake: &FakeDns) -> Option<Target> {
    if is_fake(dst) {
        return fake.name_of(dst).map(|n| Target::Host(n.to_string()));
    }
    let local = dst.is_unspecified()
        || dst.is_loopback()
        || dst.is_private()
        || dst.is_link_local()
        || dst.is_broadcast()
        || dst.is_multicast()
        || dst.is_documentation();
    (!local).then_some(Target::Addr(dst))
}

fn socks_connect_request(target: &Target, port: u16) -> Vec<u8> {
    let mut req = vec![5, 1, 0];
    match target {
        Target::Addr(ip) => {
            req.push(1);
            req.extend_from_slice(&ip.octets());
        }
        Target::Host(name) => {
            req.push(3);
            req.push(name.len().min(255) as u8);
            req.extend_from_slice(&name.as_bytes()[..name.len().min(255)]);
        }
    }
    req.extend_from_slice(&port.to_be_bytes());
    req
}

fn socks_auth_request(user: &str, pass: &str) -> Vec<u8> {
    let mut req = vec![1, user.len() as u8];
    req.extend_from_slice(user.as_bytes());
    req.push(pass.len() as u8);
    req.extend_from_slice(pass.as_bytes());
    req
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Greeting,
    Auth,
    Connect,
    Relay,
}

/// How much of a SOCKS5 CONNECT reply `buf` holds: `Some(len)` once the
/// whole reply is there.
fn socks_reply_len(buf: &[u8]) -> Option<usize> {
    let len = match *buf.get(3)? {
        1 => 10,
        4 => 22,
        3 => 7 + *buf.get(4)? as usize,
        _ => return Some(buf.len().max(4)), // malformed; the reply code decides
    };
    (buf.len() >= len).then_some(len)
}

/// One guest connection being relayed.
struct Flow {
    handle: SocketHandle,
    target: Target,
    port: u16,
    tor: Option<TcpStream>,
    phase: Phase,
    /// Waiting to be written to Tor: handshake messages, then the guest's data.
    out: Vec<u8>,
    /// Handshake reply bytes read so far.
    handshake_in: Vec<u8>,
    /// Waiting to be given to the guest.
    to_guest: Vec<u8>,
    started: StdInstant,
    tor_eof: bool,
    shutdown_sent: bool,
    identity: u32,
}

/// What [`Flow::step`] needs from outside.
struct StepCtx<'a> {
    tor_port: Option<u16>,
    user: &'a str,
    stats: &'a GatewayStats,
}

impl Flow {
    fn wants_write(&self) -> bool {
        !self.out.is_empty()
    }

    /// Moves data as far as it will go right now. `false`: the flow is over
    /// and its socket should be aborted.
    fn step(&mut self, sock: &mut tcp::Socket<'_>, ctx: &StepCtx<'_>) -> bool {
        if matches!(sock.state(), tcp::State::Closed | tcp::State::TimeWait) {
            return false;
        }
        if sock.state() != tcp::State::Established
            && self.tor.is_none()
            && !matches!(
                sock.state(),
                tcp::State::CloseWait | tcp::State::FinWait1 | tcp::State::FinWait2
            )
        {
            // Still in the guest's TCP handshake.
            return true;
        }
        if self.phase != Phase::Relay && self.started.elapsed() > CONNECT_TIMEOUT {
            debug!(target = ?self.target, "Tor connection timed out");
            return false;
        }
        if self.tor.is_none() {
            let Some(port) = ctx.tor_port else {
                return false;
            };
            let addr = SocketAddr::from(([127, 0, 0, 1], port));
            let Ok(stream) = TcpStream::connect_timeout(&addr, Duration::from_secs(1)) else {
                return false;
            };
            let _ = stream.set_nonblocking(true);
            let _ = stream.set_nodelay(true);
            self.tor = Some(stream);
            // Greeting: version 5, one method, username/password.
            self.out.extend_from_slice(&[5, 1, 2]);
        }
        if !self.write_to_tor() {
            return false;
        }
        if !self.read_from_tor(sock, ctx) {
            return false;
        }
        if self.phase == Phase::Relay {
            // Guest -> Tor.
            while self.out.len() < CHUNK * 2 && sock.can_recv() {
                let mut buf = [0u8; CHUNK];
                match sock.recv_slice(&mut buf) {
                    Ok(n) if n > 0 => {
                        self.out.extend_from_slice(&buf[..n]);
                        ctx.stats.bytes_up.fetch_add(n as u64, Ordering::Relaxed);
                    }
                    _ => break,
                }
            }
            if !self.write_to_tor() {
                return false;
            }
            if !sock.may_recv()
                && sock.recv_queue() == 0
                && self.out.is_empty()
                && !self.shutdown_sent
            {
                self.shutdown_sent = true;
                if let Some(tor) = &self.tor {
                    let _ = tor.shutdown(Shutdown::Write);
                }
            }
            // Tor -> guest.
            if !self.to_guest.is_empty() && sock.can_send() {
                match sock.send_slice(&self.to_guest) {
                    Ok(n) => {
                        self.to_guest.drain(..n);
                    }
                    Err(_) => return false,
                }
            }
            if self.tor_eof && self.to_guest.is_empty() && sock.may_send() {
                sock.close();
            }
        }
        true
    }

    fn write_to_tor(&mut self) -> bool {
        let Some(tor) = self.tor.as_mut() else {
            return true;
        };
        while !self.out.is_empty() {
            match tor.write(&self.out) {
                Ok(0) => return false,
                Ok(n) => {
                    self.out.drain(..n);
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => return false,
            }
        }
        true
    }

    fn read_from_tor(&mut self, sock: &tcp::Socket<'_>, ctx: &StepCtx<'_>) -> bool {
        let Some(tor) = self.tor.as_mut() else {
            return true;
        };
        if self.phase == Phase::Relay {
            if self.tor_eof || !self.to_guest.is_empty() || !sock.may_send() {
                return true;
            }
            let mut buf = [0u8; CHUNK];
            return match tor.read(&mut buf) {
                Ok(0) => {
                    self.tor_eof = true;
                    true
                }
                Ok(n) => {
                    self.to_guest.extend_from_slice(&buf[..n]);
                    ctx.stats.bytes_down.fetch_add(n as u64, Ordering::Relaxed);
                    true
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => true,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => true,
                Err(_) => false,
            };
        }
        let mut buf = [0u8; 512];
        match tor.read(&mut buf) {
            Ok(0) => return false,
            Ok(n) => self.handshake_in.extend_from_slice(&buf[..n]),
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) => {}
            Err(_) => return false,
        }
        self.advance_handshake(ctx)
    }

    /// Consumes the replies Tor has sent so far and queues the next message.
    fn advance_handshake(&mut self, ctx: &StepCtx<'_>) -> bool {
        loop {
            match self.phase {
                Phase::Greeting => {
                    if self.handshake_in.len() < 2 {
                        return true;
                    }
                    if self.handshake_in[..2] != [5, 2] {
                        return false;
                    }
                    self.handshake_in.drain(..2);
                    // The password only has to be the same for connections
                    // that may share a circuit: it changes with the identity.
                    let pass = format!("identity-{}", self.identity);
                    self.out.extend(socks_auth_request(ctx.user, &pass));
                    self.phase = Phase::Auth;
                }
                Phase::Auth => {
                    if self.handshake_in.len() < 2 {
                        return true;
                    }
                    if self.handshake_in[..2] != [1, 0] {
                        return false;
                    }
                    self.handshake_in.drain(..2);
                    self.out
                        .extend(socks_connect_request(&self.target, self.port));
                    self.phase = Phase::Connect;
                }
                Phase::Connect => {
                    let Some(len) = socks_reply_len(&self.handshake_in) else {
                        return true;
                    };
                    if self.handshake_in.len() < 2 || self.handshake_in[1] != 0 {
                        debug!(target = ?self.target, reply = ?self.handshake_in.get(1), "Tor refused the connection");
                        return false;
                    }
                    // Anything after the reply is already the stream.
                    let rest = self
                        .handshake_in
                        .split_off(len.min(self.handshake_in.len()));
                    self.to_guest.extend(rest);
                    self.handshake_in.clear();
                    self.phase = Phase::Relay;
                    return true;
                }
                Phase::Relay => return true,
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The serving loop
// ---------------------------------------------------------------------------

fn new_tcp_socket() -> tcp::Socket<'static> {
    let mut socket = tcp::Socket::new(
        tcp::SocketBuffer::new(vec![0; FLOW_BUFFER]),
        tcp::SocketBuffer::new(vec![0; FLOW_BUFFER]),
    );
    socket.set_nagle_enabled(false);
    socket.set_keep_alive(Some(smoltcp::time::Duration::from_secs(60)));
    socket
}

fn new_udp_socket() -> udp::Socket<'static> {
    udp::Socket::new(
        udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY; 16], vec![0; 16 * 1500]),
        udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY; 16], vec![0; 16 * 1500]),
    )
}

struct Serve {
    id: u64,
    listener: UnixListener,
    conn: Option<UnixStream>,
    reader: FrameReader,
    outbuf: VecDeque<u8>,
    tor: TorEndpoint,
    stats: Arc<GatewayStats>,
    cancel: Arc<AtomicBool>,
    started: StdInstant,
    device: QueueDevice,
    iface: Interface,
    sockets: SocketSet<'static>,
    dns_socket: SocketHandle,
    listeners: HashMap<u16, SocketHandle>,
    flows: Vec<Flow>,
    /// Sockets aborted this round: removed after the next poll has sent their
    /// resets.
    dying: Vec<SocketHandle>,
    fake: FakeDns,
    seen_identity: u32,
}

impl Serve {
    fn new(
        id: u64,
        listener: UnixListener,
        tor: TorEndpoint,
        stats: Arc<GatewayStats>,
        cancel: Arc<AtomicBool>,
    ) -> Self {
        let mut device = QueueDevice::default();
        let mut config = Config::new(HardwareAddress::Ethernet(EthernetAddress(GATEWAY_MAC)));
        config.random_seed = id ^ 0x1c0_ffee;
        let mut iface = Interface::new(config, &mut device, Instant::from_millis(0));
        iface.update_ip_addrs(|addrs| {
            let _ = addrs.push(IpCidr::new(IpAddress::Ipv4(GATEWAY_IP), 24));
        });
        // Accept connections to every address, and answer from it.
        iface.set_any_ip(true);
        let _ = iface.routes_mut().add_default_ipv4_route(GATEWAY_IP);

        let mut sockets = SocketSet::new(Vec::new());
        let mut dns = new_udp_socket();
        let _ = dns.bind(53);
        let dns_socket = sockets.add(dns);
        let seen_identity = stats.identity.load(Ordering::SeqCst);
        Self {
            id,
            listener,
            conn: None,
            reader: FrameReader::default(),
            outbuf: VecDeque::new(),
            tor,
            stats,
            cancel,
            started: StdInstant::now(),
            device,
            iface,
            sockets,
            dns_socket,
            listeners: HashMap::new(),
            flows: Vec::new(),
            dying: Vec::new(),
            fake: FakeDns::default(),
            seen_identity,
        }
    }

    fn now(&self) -> Instant {
        Instant::from_millis(self.started.elapsed().as_millis() as i64)
    }

    fn run(&mut self) {
        while !self.cancel.load(Ordering::SeqCst) {
            let timeout = self.wait();
            let _ = timeout;
            self.accept();
            self.read_guest();
            self.round();
            self.write_guest();
        }
        // Dropping the sockets and the unix stream ends everything.
        debug!(id = self.id, "Tor gateway stopped");
    }

    /// Waits until something can happen; returns how long it waited for.
    fn wait(&mut self) -> Duration {
        let delay = self
            .iface
            .poll_delay(self.now(), &self.sockets)
            .map(|d| Duration::from_micros(d.total_micros()))
            .unwrap_or(Duration::from_secs(1));
        let cap = if self.flows.is_empty() {
            Duration::from_secs(1)
        } else {
            Duration::from_millis(20)
        };
        let timeout = delay.min(cap);

        let tor_fds: Vec<BorrowedFd<'_>> = self
            .flows
            .iter()
            .filter_map(|f| f.tor.as_ref().map(|t| t.as_fd()))
            .collect();
        let write_wanted: Vec<bool> = self
            .flows
            .iter()
            .filter(|f| f.tor.is_some())
            .map(Flow::wants_write)
            .collect();
        let mut fds = vec![PollFd::new(&self.listener, PollFlags::IN)];
        if let Some(conn) = &self.conn {
            let mut flags = PollFlags::IN;
            if !self.outbuf.is_empty() {
                flags |= PollFlags::OUT;
            }
            fds.push(PollFd::new(conn, flags));
        }
        for (fd, write) in tor_fds.iter().zip(write_wanted) {
            fds.push(PollFd::from_borrowed_fd(
                *fd,
                if write {
                    PollFlags::IN | PollFlags::OUT
                } else {
                    PollFlags::IN
                },
            ));
        }
        let spec = rustix::event::Timespec {
            tv_sec: timeout.as_secs() as _,
            tv_nsec: timeout.subsec_nanos() as _,
        };
        let _ = rustix::event::poll(&mut fds, Some(&spec));
        timeout
    }

    /// A new connection from QEMU replaces the old one (QEMU reconnects after
    /// a restart).
    fn accept(&mut self) {
        if let Ok((stream, _)) = self.listener.accept()
            && stream.set_nonblocking(true).is_ok()
        {
            self.conn = Some(stream);
            self.reader = FrameReader::default();
            self.outbuf.clear();
        }
    }

    /// Reads what the guest sent and runs each frame through the stack.
    fn read_guest(&mut self) {
        let Some(conn) = self.conn.as_mut() else {
            return;
        };
        let mut buf = [0u8; 64 * 1024];
        loop {
            match conn.read(&mut buf) {
                Ok(0) => {
                    self.conn = None;
                    return;
                }
                Ok(n) => self.reader.push(&buf[..n]),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => {
                    self.conn = None;
                    return;
                }
            }
        }
        loop {
            match self.reader.next_frame() {
                Ok(Some(frame)) => self.ingest(frame),
                Ok(None) => break,
                Err(()) => {
                    warn!(
                        id = self.id,
                        "Tor gateway: the guest link is out of step; dropping it"
                    );
                    self.conn = None;
                    return;
                }
            }
        }
    }

    /// Feeds one frame to the stack (one at a time, so a listener can be put
    /// in place for each connection attempt before it is processed).
    fn ingest(&mut self, frame: Vec<u8>) {
        match classify(&frame) {
            Ingress::Dhcp(payload) => {
                if let Some(reply) = dhcp_reply(&payload) {
                    self.device.tx.push_back(reply);
                }
                return;
            }
            Ingress::TcpSyn { dst, port } => {
                // Refused here, before the stack accepts it: a connection
                // that is accepted and then reset looks, for a moment, like
                // one that worked.
                let refuse = self.tor.get().is_none()
                    || self.flows.len() >= MAX_FLOWS
                    || target_for(dst, &self.fake).is_none();
                if refuse {
                    if let Some(reset) = tcp_reset(&frame) {
                        self.device.tx.push_back(reset);
                    }
                    return;
                }
                self.ensure_listener(port)
            }
            Ingress::Pass => {}
            Ingress::Drop => return,
        }
        self.device.rx.push_back(frame);
        let now = self.now();
        self.iface.poll(now, &mut self.device, &mut self.sockets);
        self.promote();
    }

    fn ensure_listener(&mut self, port: u16) {
        if self.listeners.contains_key(&port)
            || self.listeners.len() >= MAX_LISTEN_PORTS
            || self.flows.len() >= MAX_FLOWS
        {
            return;
        }
        let mut socket = new_tcp_socket();
        if socket.listen(port).is_ok() {
            let handle = self.sockets.add(socket);
            self.listeners.insert(port, handle);
        }
    }

    /// Listeners that got a connection become flows.
    fn promote(&mut self) {
        let connected: Vec<u16> = self
            .listeners
            .iter()
            .filter(|&(_, &h)| self.sockets.get::<tcp::Socket<'_>>(h).state() != tcp::State::Listen)
            .map(|(&port, _)| port)
            .collect();
        for port in connected {
            let Some(handle) = self.listeners.remove(&port) else {
                continue;
            };
            let sock = self.sockets.get_mut::<tcp::Socket<'_>>(handle);
            let dst = match sock.local_endpoint() {
                Some(IpEndpoint {
                    addr: IpAddress::Ipv4(ip),
                    ..
                }) => ip,
                _ => {
                    sock.abort();
                    self.dying.push(handle);
                    continue;
                }
            };
            match target_for(dst, &self.fake) {
                Some(target) => self.flows.push(Flow {
                    handle,
                    target,
                    port,
                    tor: None,
                    phase: Phase::Greeting,
                    out: Vec::new(),
                    handshake_in: Vec::new(),
                    to_guest: Vec::new(),
                    started: StdInstant::now(),
                    tor_eof: false,
                    shutdown_sent: false,
                    identity: self.stats.identity.load(Ordering::SeqCst),
                }),
                None => {
                    sock.abort();
                    self.dying.push(handle);
                }
            }
        }
    }

    /// One pass over everything that isn't a frame arriving: flows, DNS, the
    /// stack's timers.
    fn round(&mut self) {
        for handle in self.dying.drain(..) {
            self.sockets.remove(handle);
        }
        let identity = self.stats.identity.load(Ordering::SeqCst);
        let reset_all = identity != self.seen_identity;
        self.seen_identity = identity;

        let ctx_port = self.tor.get();
        let user = format!("ironland-vm{}", self.id);
        let mut kept = Vec::with_capacity(self.flows.len());
        for mut flow in std::mem::take(&mut self.flows) {
            let sock = self.sockets.get_mut::<tcp::Socket<'_>>(flow.handle);
            let ctx = StepCtx {
                tor_port: ctx_port,
                user: &user,
                stats: &self.stats,
            };
            if !reset_all && flow.step(sock, &ctx) {
                kept.push(flow);
            } else {
                sock.abort();
                self.dying.push(flow.handle);
            }
        }
        self.flows = kept;
        self.stats
            .flows
            .store(self.flows.len() as u32, Ordering::Relaxed);

        self.serve_dns();
        let now = self.now();
        self.iface.poll(now, &mut self.device, &mut self.sockets);
        self.promote();
        // Resets sent by that poll are out; the sockets can go.
        for handle in self.dying.drain(..) {
            self.sockets.remove(handle);
        }
    }

    fn serve_dns(&mut self) {
        let socket = self.sockets.get_mut::<udp::Socket<'_>>(self.dns_socket);
        while let Ok((data, meta)) = socket.recv() {
            let Some(answer) = dns_answer(data, &mut self.fake) else {
                continue;
            };
            let reply = udp::UdpMetadata {
                endpoint: meta.endpoint,
                local_address: meta.local_address,
                meta: PacketMeta::default(),
            };
            let _ = socket.send_slice(&answer, reply);
        }
    }

    /// Sends what the stack produced to the guest.
    fn write_guest(&mut self) {
        while let Some(frame) = self.device.tx.pop_front() {
            if self.conn.is_some() && self.outbuf.len() < 8 * 1024 * 1024 {
                frame_for_stream(&frame, &mut self.outbuf);
            }
        }
        let Some(conn) = self.conn.as_mut() else {
            self.outbuf.clear();
            return;
        };
        while !self.outbuf.is_empty() {
            let (head, _) = self.outbuf.as_slices();
            match conn.write(head) {
                Ok(0) => break,
                Ok(n) => {
                    self.outbuf.drain(..n);
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => {
                    self.conn = None;
                    self.outbuf.clear();
                    return;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_are_cut_from_the_stream_at_their_length() {
        let mut out = VecDeque::new();
        frame_for_stream(&[1, 2, 3], &mut out);
        frame_for_stream(&[9; 5], &mut out);
        let bytes: Vec<u8> = out.into_iter().collect();

        let mut reader = FrameReader::default();
        // Arrives in awkward pieces.
        reader.push(&bytes[..2]);
        assert_eq!(reader.next_frame(), Ok(None));
        reader.push(&bytes[2..9]);
        assert_eq!(reader.next_frame(), Ok(Some(vec![1, 2, 3])));
        assert_eq!(reader.next_frame(), Ok(None));
        reader.push(&bytes[9..]);
        assert_eq!(reader.next_frame(), Ok(Some(vec![9; 5])));
    }

    #[test]
    fn a_nonsense_frame_length_is_an_error() {
        let mut reader = FrameReader::default();
        reader.push(&[0xff, 0xff, 0xff, 0xff, 0]);
        assert_eq!(reader.next_frame(), Err(()));
        let mut reader = FrameReader::default();
        reader.push(&[0, 0, 0, 0]);
        assert_eq!(reader.next_frame(), Err(()));
    }

    fn dns_query(name: &str, qtype: u16) -> Vec<u8> {
        let mut q = vec![0x12, 0x34, 0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0];
        for label in name.split('.') {
            q.push(label.len() as u8);
            q.extend_from_slice(label.as_bytes());
        }
        q.push(0);
        q.extend_from_slice(&qtype.to_be_bytes());
        q.extend_from_slice(&[0, 1]);
        q
    }

    fn answer_ip(answer: &[u8]) -> Ipv4Addr {
        let n = answer.len();
        Ipv4Addr::new(answer[n - 4], answer[n - 3], answer[n - 2], answer[n - 1])
    }

    #[test]
    fn a_names_gets_a_fake_address_that_leads_back_to_it() {
        let mut fake = FakeDns::default();
        let answer = dns_answer(&dns_query("Example.COM", QTYPE_A), &mut fake).unwrap();
        assert_eq!(&answer[..2], [0x12, 0x34], "same id");
        assert_eq!(answer[2] & 0x80, 0x80, "a response");
        assert_eq!(answer[3] & 0x0f, 0, "no error");
        assert_eq!(&answer[6..8], [0, 1], "one answer");
        let ip = answer_ip(&answer);
        assert!(is_fake(ip), "{ip}");
        assert_eq!(fake.name_of(ip), Some("example.com"));
        // The same name, in any case, keeps its address.
        let again = dns_answer(&dns_query("example.com", QTYPE_A), &mut fake).unwrap();
        assert_eq!(answer_ip(&again), ip);
        let other = dns_answer(&dns_query("example.org", QTYPE_A), &mut fake).unwrap();
        assert_ne!(answer_ip(&other), ip);
    }

    #[test]
    fn ipv6_and_other_record_kinds_are_answered_with_nothing() {
        let mut fake = FakeDns::default();
        for qtype in [QTYPE_AAAA, 15, 16] {
            let answer = dns_answer(&dns_query("example.com", qtype), &mut fake).unwrap();
            assert_eq!(answer[3] & 0x0f, 0, "no error for type {qtype}");
            assert_eq!(&answer[6..8], [0, 0], "no answers for type {qtype}");
        }
        assert!(fake.by_name.is_empty(), "nothing was handed out");
    }

    #[test]
    fn reverse_lookups_say_the_name_does_not_exist() {
        let mut fake = FakeDns::default();
        let answer =
            dns_answer(&dns_query("1.0.0.127.in-addr.arpa", QTYPE_PTR), &mut fake).unwrap();
        assert_eq!(answer[3] & 0x0f, 3);
    }

    #[test]
    fn what_is_not_a_query_is_not_answered() {
        let mut fake = FakeDns::default();
        assert_eq!(dns_answer(&[], &mut fake), None);
        assert_eq!(dns_answer(&[0; 11], &mut fake), None);
        let mut response = dns_query("example.com", QTYPE_A);
        response[2] |= 0x80;
        assert_eq!(dns_answer(&response, &mut fake), None);
        let mut truncated = dns_query("example.com", QTYPE_A);
        truncated.truncate(20);
        assert_eq!(dns_answer(&truncated, &mut fake), None);
    }

    #[test]
    fn a_reused_fake_address_forgets_its_old_name() {
        let mut fake = FakeDns::default();
        let first = fake.address_for("first.example");
        // Come all the way around.
        for i in 0..FAKE_NAMES {
            fake.address_for(&format!("n{i}.example"));
        }
        assert_eq!(
            fake.name_of(first),
            Some("n".to_string() + &(FAKE_NAMES - 1).to_string() + ".example").as_deref()
        );
        assert!(!fake.by_name.contains_key("first.example"));
        assert!(fake.by_name.len() <= FAKE_NAMES as usize);
    }

    #[test]
    fn connections_go_to_the_name_or_the_public_address() {
        let mut fake = FakeDns::default();
        let ip = fake.address_for("example.com");
        assert_eq!(
            target_for(ip, &fake),
            Some(Target::Host("example.com".into()))
        );
        let public = Ipv4Addr::new(93, 184, 216, 34);
        assert_eq!(target_for(public, &fake), Some(Target::Addr(public)));
    }

    #[test]
    fn connections_that_could_only_mean_this_network_go_nowhere() {
        let fake = FakeDns::default();
        for ip in [
            Ipv4Addr::new(10, 0, 4, 2),
            Ipv4Addr::new(10, 0, 2, 2),
            Ipv4Addr::new(192, 168, 1, 1),
            Ipv4Addr::new(127, 0, 0, 1),
            Ipv4Addr::new(169, 254, 169, 254),
            Ipv4Addr::new(224, 0, 0, 1),
            Ipv4Addr::BROADCAST,
            Ipv4Addr::UNSPECIFIED,
            // A made-up address nobody was given.
            Ipv4Addr::new(10, 200, 0, 1),
        ] {
            assert_eq!(target_for(ip, &fake), None, "{ip}");
        }
    }

    #[test]
    fn socks_requests_name_the_target() {
        assert_eq!(
            socks_connect_request(&Target::Addr(Ipv4Addr::new(1, 2, 3, 4)), 443),
            [5, 1, 0, 1, 1, 2, 3, 4, 1, 187]
        );
        let by_name = socks_connect_request(&Target::Host("a.onion".into()), 80);
        assert_eq!(&by_name[..5], [5, 1, 0, 3, 7]);
        assert_eq!(&by_name[5..12], b"a.onion");
        assert_eq!(&by_name[12..], [0, 80]);
        assert_eq!(socks_auth_request("u", "pw"), [1, 1, b'u', 2, b'p', b'w']);
    }

    #[test]
    fn a_socks_reply_is_complete_when_its_address_is() {
        assert_eq!(socks_reply_len(&[5, 0, 0]), None);
        assert_eq!(socks_reply_len(&[5, 0, 0, 1, 0, 0, 0, 0]), None);
        assert_eq!(socks_reply_len(&[5, 0, 0, 1, 0, 0, 0, 0, 0, 0]), Some(10));
        assert_eq!(
            socks_reply_len(&[5, 0, 0, 3, 3, b'a', b'b', b'c', 0, 0]),
            Some(10)
        );
        assert_eq!(socks_reply_len(&[5, 0, 0, 4]), None);
    }

    fn dhcp_request(kind: DhcpMessageType, requested: Option<Ipv4Addr>) -> Vec<u8> {
        let repr = DhcpRepr {
            message_type: kind,
            transaction_id: 0xabcd,
            secs: 0,
            client_hardware_address: EthernetAddress([2, 0, 0, 0, 0, 9]),
            client_ip: Ipv4Addr::UNSPECIFIED,
            your_ip: Ipv4Addr::UNSPECIFIED,
            server_ip: Ipv4Addr::UNSPECIFIED,
            router: None,
            subnet_mask: None,
            relay_agent_ip: Ipv4Addr::UNSPECIFIED,
            broadcast: true,
            requested_ip: requested,
            client_identifier: None,
            server_identifier: None,
            parameter_request_list: None,
            dns_servers: None,
            max_size: None,
            lease_duration: None,
            renew_duration: None,
            rebind_duration: None,
            additional_options: &[],
        };
        let mut bytes = vec![0u8; repr.buffer_len()];
        repr.emit(&mut DhcpPacket::new_unchecked(&mut bytes[..]))
            .unwrap();
        bytes
    }

    /// The DHCP message inside a reply frame.
    fn dhcp_of(
        frame: &[u8],
    ) -> (
        DhcpMessageType,
        Ipv4Addr,
        Option<Ipv4Addr>,
        Option<Ipv4Addr>,
    ) {
        // A reply is addressed to the client port, which no client message is.
        assert_eq!(classify(frame), Ingress::Drop);
        let eth = EthernetFrame::new_checked(frame).unwrap();
        let ip = Ipv4Packet::new_checked(eth.payload()).unwrap();
        let udp = UdpPacket::new_checked(ip.payload()).unwrap();
        assert_eq!((udp.src_port(), udp.dst_port()), (67, 68));
        let packet = DhcpPacket::new_checked(udp.payload()).unwrap();
        let repr = DhcpRepr::parse(&packet).unwrap();
        (
            repr.message_type,
            repr.your_ip,
            repr.router,
            repr.dns_servers.and_then(|d| d.first().copied()),
        )
    }

    #[test]
    fn a_dhcp_client_is_offered_and_given_the_guest_address() {
        let offer = dhcp_reply(&dhcp_request(DhcpMessageType::Discover, None)).unwrap();
        assert_eq!(
            dhcp_of(&offer),
            (
                DhcpMessageType::Offer,
                GUEST_IP,
                Some(GATEWAY_IP),
                Some(GATEWAY_IP)
            )
        );
        let ack = dhcp_reply(&dhcp_request(DhcpMessageType::Request, Some(GUEST_IP))).unwrap();
        assert_eq!(dhcp_of(&ack).0, DhcpMessageType::Ack);
    }

    #[test]
    fn a_dhcp_client_asking_for_another_address_is_refused() {
        let nak = dhcp_reply(&dhcp_request(
            DhcpMessageType::Request,
            Some(Ipv4Addr::new(192, 168, 0, 7)),
        ))
        .unwrap();
        let (kind, your_ip, router, dns) = dhcp_of(&nak);
        assert_eq!(kind, DhcpMessageType::Nak);
        assert!(your_ip.is_unspecified());
        assert_eq!((router, dns), (None, None));
    }

    #[test]
    fn other_dhcp_messages_and_garbage_get_no_answer() {
        assert_eq!(
            dhcp_reply(&dhcp_request(DhcpMessageType::Release, None)),
            None
        );
        assert_eq!(
            dhcp_reply(&dhcp_request(DhcpMessageType::Decline, None)),
            None
        );
        assert_eq!(dhcp_reply(&[1, 2, 3]), None);
    }

    fn arp_request_for(target: Ipv4Addr) -> Vec<u8> {
        let eth = EthernetRepr {
            src_addr: EthernetAddress([2, 0, 0, 0, 0, 9]),
            dst_addr: EthernetAddress::BROADCAST,
            ethertype: EthernetProtocol::Arp,
        };
        let arp = ArpRepr::EthernetIpv4 {
            operation: smoltcp::wire::ArpOperation::Request,
            source_hardware_addr: EthernetAddress([2, 0, 0, 0, 0, 9]),
            source_protocol_addr: Ipv4Addr::UNSPECIFIED,
            target_hardware_addr: EthernetAddress([0; 6]),
            target_protocol_addr: target,
        };
        let mut frame = vec![0u8; eth.buffer_len() + arp.buffer_len()];
        eth.emit(&mut EthernetFrame::new_unchecked(&mut frame[..]));
        arp.emit(&mut ArpPacket::new_unchecked(
            &mut frame[eth.buffer_len()..],
        ));
        frame
    }

    #[test]
    fn frames_are_sorted_into_dhcp_syns_and_what_the_stack_may_see() {
        let udp = |port| {
            udp_frame(
                (GUEST_IP, 1234),
                (GATEWAY_IP, port),
                EthernetAddress(GATEWAY_MAC),
                &[1, 2, 3],
            )
        };
        let dhcp = udp_frame(
            (Ipv4Addr::UNSPECIFIED, 68),
            (Ipv4Addr::BROADCAST, 67),
            EthernetAddress::BROADCAST,
            &[1, 2, 3],
        );
        assert_eq!(classify(&dhcp), Ingress::Dhcp(vec![1, 2, 3]));
        assert_eq!(classify(&udp(53)), Ingress::Pass, "DNS");
        assert_eq!(classify(&udp(123)), Ingress::Drop, "other UDP goes nowhere");
        assert_eq!(classify(&[0; 5]), Ingress::Drop);
    }

    /// A TCP SYN from the guest to `dst:port`.
    fn syn_to(dst: Ipv4Addr, port: u16) -> Vec<u8> {
        let tcp = TcpRepr {
            src_port: 50000,
            dst_port: port,
            control: TcpControl::Syn,
            seq_number: smoltcp::wire::TcpSeqNumber(1000),
            ack_number: None,
            window_len: 1000,
            window_scale: None,
            max_seg_size: None,
            sack_permitted: false,
            sack_ranges: [None; 3],
            timestamp: None,
            payload: &[],
        };
        let caps = ChecksumCapabilities::default();
        let eth = EthernetRepr {
            src_addr: EthernetAddress([2, 0, 0, 0, 0, 9]),
            dst_addr: EthernetAddress(GATEWAY_MAC),
            ethertype: EthernetProtocol::Ipv4,
        };
        let ip = Ipv4Repr {
            src_addr: GUEST_IP,
            dst_addr: dst,
            next_header: IpProtocol::Tcp,
            payload_len: tcp.buffer_len(),
            hop_limit: 64,
        };
        let mut frame = vec![0u8; eth.buffer_len() + ip.buffer_len() + tcp.buffer_len()];
        eth.emit(&mut EthernetFrame::new_unchecked(&mut frame[..]));
        ip.emit(
            &mut Ipv4Packet::new_unchecked(&mut frame[eth.buffer_len()..]),
            &caps,
        );
        tcp.emit(
            &mut TcpPacket::new_unchecked(&mut frame[eth.buffer_len() + ip.buffer_len()..]),
            &IpAddress::Ipv4(GUEST_IP),
            &IpAddress::Ipv4(dst),
            &caps,
        );
        frame
    }

    #[test]
    fn a_syn_is_classified_with_its_destination_and_port() {
        let dst = Ipv4Addr::new(93, 184, 216, 34);
        assert_eq!(
            classify(&syn_to(dst, 443)),
            Ingress::TcpSyn { dst, port: 443 }
        );
    }

    #[test]
    fn a_refused_syn_is_answered_with_a_reset_from_the_address_it_was_sent_to() {
        let dst = Ipv4Addr::new(192, 168, 1, 1);
        let reset = tcp_reset(&syn_to(dst, 22)).unwrap();
        let eth = EthernetFrame::new_checked(&reset[..]).unwrap();
        assert_eq!(
            eth.dst_addr(),
            EthernetAddress([2, 0, 0, 0, 0, 9]),
            "to the guest"
        );
        let ip = Ipv4Packet::new_checked(eth.payload()).unwrap();
        assert!(ip.verify_checksum());
        assert_eq!((ip.src_addr(), ip.dst_addr()), (dst, GUEST_IP));
        let tcp = TcpPacket::new_checked(ip.payload()).unwrap();
        assert!(tcp.verify_checksum(&IpAddress::Ipv4(dst), &IpAddress::Ipv4(GUEST_IP)));
        assert!(tcp.rst() && tcp.ack() && !tcp.syn());
        assert_eq!((tcp.src_port(), tcp.dst_port()), (22, 50000));
        assert_eq!(tcp.ack_number(), smoltcp::wire::TcpSeqNumber(1001));
        assert_eq!(tcp_reset(&[1, 2, 3]), None);
    }

    #[test]
    fn arp_is_only_for_the_gateways_own_address() {
        // The guest asking where the gateway is.
        assert_eq!(classify(&arp_request_for(GATEWAY_IP)), Ingress::Pass);
        // A DHCP client probing the address it was offered must hear nothing,
        // or it takes the address for taken and declines it.
        assert_eq!(classify(&arp_request_for(GUEST_IP)), Ingress::Drop);
        assert_eq!(
            classify(&arp_request_for(Ipv4Addr::new(8, 8, 8, 8))),
            Ingress::Drop
        );
    }

    // --- the whole stack, with a smoltcp guest and a fake Tor --------------

    use std::{net::TcpListener, thread::JoinHandle};

    /// A guest on the far end of the gateway's socket, with its own stack.
    struct TestGuest {
        conn: UnixStream,
        reader: FrameReader,
        device: QueueDevice,
        iface: Interface,
        sockets: SocketSet<'static>,
        started: StdInstant,
    }

    impl TestGuest {
        fn connect(path: &Path) -> Self {
            let conn = UnixStream::connect(path).unwrap();
            conn.set_nonblocking(true).unwrap();
            let mut device = QueueDevice::default();
            let config = Config::new(HardwareAddress::Ethernet(EthernetAddress([
                2, 0, 0, 0, 0, 9,
            ])));
            let mut iface = Interface::new(config, &mut device, Instant::from_millis(0));
            iface.update_ip_addrs(|a| {
                let _ = a.push(IpCidr::new(IpAddress::Ipv4(GUEST_IP), 24));
            });
            iface
                .routes_mut()
                .add_default_ipv4_route(GATEWAY_IP)
                .unwrap();
            Self {
                conn,
                reader: FrameReader::default(),
                device,
                iface,
                sockets: SocketSet::new(Vec::new()),
                started: StdInstant::now(),
            }
        }

        fn now(&self) -> Instant {
            Instant::from_millis(self.started.elapsed().as_millis() as i64)
        }

        /// Exchanges frames with the gateway once.
        fn pump(&mut self) {
            let mut buf = [0u8; 65536];
            if let Ok(n) = self.conn.read(&mut buf) {
                self.reader.push(&buf[..n]);
            }
            while let Ok(Some(frame)) = self.reader.next_frame() {
                self.device.rx.push_back(frame);
            }
            let now = self.now();
            self.iface.poll(now, &mut self.device, &mut self.sockets);
            let mut out = VecDeque::new();
            while let Some(frame) = self.device.tx.pop_front() {
                frame_for_stream(&frame, &mut out);
            }
            let bytes: Vec<u8> = out.into_iter().collect();
            if !bytes.is_empty() {
                // The socket is non-blocking; frames are small.
                let mut at = 0;
                while at < bytes.len() {
                    match self.conn.write(&bytes[at..]) {
                        Ok(n) => at += n,
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(1))
                        }
                        Err(e) => panic!("{e}"),
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(2));
        }

        fn until(&mut self, what: &str, mut done: impl FnMut(&mut Self) -> bool) {
            let deadline = StdInstant::now() + Duration::from_secs(10);
            while !done(self) {
                assert!(StdInstant::now() < deadline, "timed out waiting for {what}");
                self.pump();
            }
        }

        /// Looks `name` up through the gateway's DNS.
        fn resolve(&mut self, name: &str) -> Ipv4Addr {
            let handle = self.sockets.add(new_udp_socket());
            self.sockets
                .get_mut::<udp::Socket<'_>>(handle)
                .bind(40000)
                .unwrap();
            self.sockets
                .get_mut::<udp::Socket<'_>>(handle)
                .send_slice(
                    &dns_query(name, QTYPE_A),
                    IpEndpoint::new(IpAddress::Ipv4(GATEWAY_IP), 53),
                )
                .unwrap();
            let mut answer = None;
            self.until("the DNS answer", |g| {
                let sock = g.sockets.get_mut::<udp::Socket<'_>>(handle);
                if let Ok((data, _)) = sock.recv() {
                    answer = Some(answer_ip(data));
                }
                answer.is_some()
            });
            answer.unwrap()
        }

        fn open(&mut self, to: Ipv4Addr, port: u16, local: u16) -> SocketHandle {
            let handle = self.sockets.add(new_tcp_socket());
            let cx = self.iface.context();
            self.sockets
                .get_mut::<tcp::Socket<'_>>(handle)
                .connect(cx, (to, port), local)
                .unwrap();
            handle
        }

        fn state(&mut self, handle: SocketHandle) -> tcp::State {
            self.sockets.get_mut::<tcp::Socket<'_>>(handle).state()
        }
    }

    /// What the fake Tor saw of one connection.
    #[derive(Debug, Default, PartialEq, Eq)]
    struct Seen {
        user: String,
        pass: String,
        request: Vec<u8>,
        data: Vec<u8>,
    }

    /// A SOCKS5 server that accepts one connection, notes what it is asked
    /// for, reads `expect` bytes and answers with `reply`.
    fn fake_tor(expect: usize, reply: &'static [u8]) -> (u16, JoinHandle<Seen>) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            let mut seen = Seen::default();
            let mut greeting = [0u8; 3];
            stream.read_exact(&mut greeting).unwrap();
            assert_eq!(greeting, [5, 1, 2]);
            stream.write_all(&[5, 2]).unwrap();
            let mut head = [0u8; 2];
            stream.read_exact(&mut head).unwrap();
            let mut user = vec![0u8; head[1] as usize];
            stream.read_exact(&mut user).unwrap();
            let mut plen = [0u8; 1];
            stream.read_exact(&mut plen).unwrap();
            let mut pass = vec![0u8; plen[0] as usize];
            stream.read_exact(&mut pass).unwrap();
            seen.user = String::from_utf8(user).unwrap();
            seen.pass = String::from_utf8(pass).unwrap();
            stream.write_all(&[1, 0]).unwrap();
            let mut fixed = [0u8; 4];
            stream.read_exact(&mut fixed).unwrap();
            let rest = match fixed[3] {
                1 => 4 + 2,
                _ => {
                    let mut len = [0u8; 1];
                    stream.read_exact(&mut len).unwrap();
                    seen.request.push(len[0]);
                    len[0] as usize + 2
                }
            };
            let mut tail = vec![0u8; rest];
            stream.read_exact(&mut tail).unwrap();
            seen.request.splice(0..0, fixed);
            seen.request.extend(tail);
            stream.write_all(&[5, 0, 0, 1, 0, 0, 0, 0, 0, 0]).unwrap();
            let mut data = vec![0u8; expect];
            stream.read_exact(&mut data).unwrap();
            seen.data = data;
            stream.write_all(reply).unwrap();
            // Stay until the gateway hangs up, so the reply isn't cut off.
            let mut rest = Vec::new();
            let _ = stream.read_to_end(&mut rest);
            seen
        });
        (port, handle)
    }

    fn socket_path(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("ivg-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("g.sock")
    }

    fn read_all(guest: &mut TestGuest, handle: SocketHandle, want: usize) -> Vec<u8> {
        let mut got = Vec::new();
        guest.until("the reply", |g| {
            let sock = g.sockets.get_mut::<tcp::Socket<'_>>(handle);
            let mut buf = [0u8; 4096];
            while let Ok(n) = sock.recv_slice(&mut buf) {
                if n == 0 {
                    break;
                }
                got.extend_from_slice(&buf[..n]);
            }
            got.len() >= want
        });
        got
    }

    #[test]
    fn a_guest_connection_to_a_resolved_name_reaches_tor_by_name() {
        let path = socket_path("name");
        let (port, tor) = fake_tor(5, b"world");
        let endpoint = TorEndpoint::default();
        endpoint.set(Some(port));
        let gateway = Gateway::spawn(7, &path, endpoint).unwrap();
        let mut guest = TestGuest::connect(&path);

        let ip = guest.resolve("example.com");
        assert!(is_fake(ip), "{ip}");
        let socket = guest.open(ip, 80, 50000);
        guest.until("the connection", |g| {
            g.state(socket) == tcp::State::Established
        });
        guest
            .sockets
            .get_mut::<tcp::Socket<'_>>(socket)
            .send_slice(b"hello")
            .unwrap();
        assert_eq!(read_all(&mut guest, socket, 5), b"world");
        assert_eq!(gateway.stats().flows.load(Ordering::Relaxed), 1);
        guest.sockets.get_mut::<tcp::Socket<'_>>(socket).close();
        guest.until("the close", |g| {
            matches!(g.state(socket), tcp::State::Closed | tcp::State::TimeWait)
        });

        let seen = tor.join().unwrap();
        assert_eq!(seen.user, "ironland-vm7");
        assert_eq!(seen.pass, "identity-1");
        // Connect by *name*, so Tor's exit does the lookup.
        let mut want = vec![5, 1, 0, 3, 11];
        want.extend_from_slice(b"example.com");
        want.extend_from_slice(&80u16.to_be_bytes());
        assert_eq!(seen.request, want);
        assert_eq!(seen.data, b"hello");
        drop(gateway);
    }

    #[test]
    fn a_guest_connection_to_a_plain_address_reaches_tor_by_address() {
        let path = socket_path("addr");
        let (port, tor) = fake_tor(2, b"ok");
        let endpoint = TorEndpoint::default();
        endpoint.set(Some(port));
        let gateway = Gateway::spawn(8, &path, endpoint).unwrap();
        let mut guest = TestGuest::connect(&path);

        let socket = guest.open(Ipv4Addr::new(93, 184, 216, 34), 443, 50001);
        guest.until("the connection", |g| {
            g.state(socket) == tcp::State::Established
        });
        guest
            .sockets
            .get_mut::<tcp::Socket<'_>>(socket)
            .send_slice(b"hi")
            .unwrap();
        assert_eq!(read_all(&mut guest, socket, 2), b"ok");
        guest.sockets.get_mut::<tcp::Socket<'_>>(socket).close();
        guest.until("the close", |g| {
            matches!(g.state(socket), tcp::State::Closed | tcp::State::TimeWait)
        });
        let seen = tor.join().unwrap();
        assert_eq!(seen.request, [5, 1, 0, 1, 93, 184, 216, 34, 1, 187]);
        drop(gateway);
    }

    #[test]
    fn a_new_identity_resets_open_connections_and_changes_the_credentials() {
        let path = socket_path("identity");
        let (port, tor) = fake_tor(1, b"x");
        let tor_endpoint = TorEndpoint::default();
        tor_endpoint.set(Some(port));
        let gateway = Gateway::spawn(9, &path, tor_endpoint.clone()).unwrap();
        let mut guest = TestGuest::connect(&path);

        let socket = guest.open(Ipv4Addr::new(93, 184, 216, 34), 80, 50002);
        guest.until("the connection", |g| {
            g.state(socket) == tcp::State::Established
        });
        guest
            .sockets
            .get_mut::<tcp::Socket<'_>>(socket)
            .send_slice(b"a")
            .unwrap();
        assert_eq!(read_all(&mut guest, socket, 1), b"x");

        gateway.new_identity();
        assert_eq!(gateway.stats().identity.load(Ordering::Relaxed), 2);
        guest.until("the reset", |g| g.state(socket) == tcp::State::Closed);
        assert_eq!(tor.join().unwrap().pass, "identity-1");

        // A connection made afterwards logs in under the new identity.
        let (port, tor) = fake_tor(1, b"y");
        tor_endpoint.set(Some(port));
        let socket = guest.open(Ipv4Addr::new(93, 184, 216, 34), 80, 50012);
        guest.until("the connection", |g| {
            g.state(socket) == tcp::State::Established
        });
        guest
            .sockets
            .get_mut::<tcp::Socket<'_>>(socket)
            .send_slice(b"b")
            .unwrap();
        assert_eq!(read_all(&mut guest, socket, 1), b"y");
        guest.sockets.get_mut::<tcp::Socket<'_>>(socket).abort();
        guest.pump();
        assert_eq!(tor.join().unwrap().pass, "identity-2");
        drop(gateway);
    }

    #[test]
    fn without_tor_a_connection_is_reset_instead_of_going_anywhere_else() {
        let path = socket_path("notor");
        let gateway = Gateway::spawn(10, &path, TorEndpoint::default()).unwrap();
        let mut guest = TestGuest::connect(&path);
        let socket = guest.open(Ipv4Addr::new(93, 184, 216, 34), 80, 50003);
        let mut established = false;
        guest.until("the reset", |g| {
            established |= g.state(socket) == tcp::State::Established;
            g.state(socket) == tcp::State::Closed
        });
        assert!(
            !established,
            "the connection looked successful for a moment"
        );
        drop(gateway);
    }

    #[test]
    fn connections_to_this_network_are_reset_without_asking_tor() {
        let path = socket_path("local");
        // A Tor that must never be contacted.
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let endpoint = TorEndpoint::default();
        endpoint.set(Some(listener.local_addr().unwrap().port()));
        let gateway = Gateway::spawn(11, &path, endpoint).unwrap();
        let mut guest = TestGuest::connect(&path);
        let socket = guest.open(Ipv4Addr::new(192, 168, 1, 1), 22, 50004);
        // Refused at the SYN: the guest's connection never looks established.
        let mut established = false;
        guest.until("the reset", |g| {
            established |= g.state(socket) == tcp::State::Established;
            g.state(socket) == tcp::State::Closed
        });
        assert!(
            !established,
            "the connection looked successful for a moment"
        );
        assert!(listener.accept().is_err(), "the gateway contacted Tor");
        drop(gateway);
    }

    #[test]
    fn dns_to_any_address_is_answered_by_the_gateway() {
        let path = socket_path("anydns");
        let gateway = Gateway::spawn(12, &path, TorEndpoint::default()).unwrap();
        let mut guest = TestGuest::connect(&path);
        // A hard-coded public resolver must not get the query.
        let handle = guest.sockets.add(new_udp_socket());
        let sock = guest.sockets.get_mut::<udp::Socket<'_>>(handle);
        sock.bind(40001).unwrap();
        sock.send_slice(
            &dns_query("example.net", QTYPE_A),
            IpEndpoint::new(IpAddress::Ipv4(Ipv4Addr::new(8, 8, 8, 8)), 53),
        )
        .unwrap();
        let mut ip = None;
        guest.until("the DNS answer", |g| {
            if let Ok((data, meta)) = g.sockets.get_mut::<udp::Socket<'_>>(handle).recv() {
                assert_eq!(
                    meta.endpoint.addr,
                    IpAddress::Ipv4(Ipv4Addr::new(8, 8, 8, 8))
                );
                ip = Some(answer_ip(data));
            }
            ip.is_some()
        });
        assert!(is_fake(ip.unwrap()));
        drop(gateway);
    }
}
