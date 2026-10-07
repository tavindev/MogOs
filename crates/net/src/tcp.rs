//! TCP (RFC 9293): MSS and window scaling (RFC 7323), retransmission timeout (RFC 6298), NewReno (RFC 5681,
//! RFC 6582) with byte counting (RFC 3465), all in caller-supplied memory.
//!
//! - A connection slot owns a receive and a send ring from the caller. Out-of-order data is written into the
//!   receive ring at its offset and tracked as up to `OOO` ranges; each segment is acknowledged at once.
//! - A passive open lives in the half-open table until the handshake completes, so a SYN flood never takes a slot.
//!   A keyed hash of the connection picks its run of `PROBES` slots, so lookups are O(1) at any table size.
//!   A full run answers with SYN cookies (the ISS encodes the MSS and a keyed hash; no window scaling), accepted
//!   only while the stack has sent cookies recently.
//! - A connection entering TIME_WAIT leaves its slot for a compact entry; a full TIME_WAIT table reuses its oldest,
//!   and a SYN above the entry's sequence number starts a new connection whose ISS is 65537 plus 24 keyed bits above
//!   the old one (RFC 9293 3.10.7.4 note, RFC 1122 4.2.2.13).
//! - Each connection has one deadline, derived from its state (`deadline`) and cached at the end of every event that
//!   can move it: retransmission, persist (given up after 10 unanswered probes), or a released connection's
//!   FIN-WAIT-2 idle limit.
//! - ISNs (RFC 6528) and ephemeral ports (RFC 6056, algorithm 3) come from SipHash-2-4; every keyed use (ISNs,
//!   ports, cookies, TIME_WAIT takeover, the half-open mix) has its own key derived from the caller's seed.
//! - RFC 5961: an inexact in-window RST, any SYN on a synchronized connection and an ACK outside the sent range get
//!   a challenge ACK, at most `CHALLENGES` per second per connection. RFC 5927: an ICMP error must name a sequence
//!   number in flight, and a hard error aborts only a connection still in SYN-SENT.
//! - Segments about a connection go to the MAC it last resolved by ARP (until its first send, a passive open's SYN
//!   source), never to a received frame's source; only a RST for an unknown connection and a SYN-ACK answer the
//!   frame's source.
//! - Congestion control is NewReno as plain code, with go-back-N after a timeout.
use core::net::{Ipv4Addr, SocketAddrV4};

use crate::{
    Counters, ETH, Error, IP, MAX_PACKET, Mac, Nic, Reason, SEC, Stack, TYPE_IPV4, fold, ip_at,
    pseudo, ring_put, ring_read, sum, unicast, unicast_mac, write_eth, write_ip,
};

pub(crate) const PROTO_TCP: u8 = 6;
const TCP: usize = 20;
const SYN_OPTIONS: usize = 8;
const FIN: u8 = 1;
const SYN: u8 = 2;
const RST: u8 = 4;
const PSH: u8 = 8;
const ACK: u8 = 16;
const MS: u64 = 1_000_000;
const INITIAL_RTO: u64 = SEC;
/// Linux's floor: RFC 6298's 1 s is a SHOULD, and 200 ms is still above any RTT the stack serves.
const MIN_RTO: u64 = 200 * MS;
const MAX_RTO: u64 = 60 * SEC;
const SYN_TRIES: u8 = 6;
const DATA_TRIES: u8 = 10;
const SYNACK_TRIES: u8 = 5;
/// Zero-window probes a released connection sends before giving up, answered or not (Linux's orphan limit).
const ORPHAN_PROBES: u8 = 8;
/// 2 MSL.
const TIME_WAIT: u64 = 60 * SEC;
/// How long a closed connection waits in FIN-WAIT-2 for the peer's FIN.
const FIN_WAIT_2: u64 = 60 * SEC;
const CHALLENGES: u8 = 10;
/// A half-open entry lives in one of this many slots in a row, from a keyed hash of its connection.
const PROBES: usize = 8;
/// At most one ACK per connection per this long answers out-of-window segments (as Linux).
const OOW_ACK: u64 = 500 * MS;
/// A SYN cookie's clock: a cookie is accepted for one to two periods.
const COOKIE_PERIOD: u64 = 16 * SEC;
/// The MSS values a cookie can carry, by its 2-bit index.
const COOKIE_MSS: [u16; 4] = [536, 1220, 1440, 1460];
/// The new ISS for a SYN taking over TIME_WAIT sits at least this far above the old connection's (RFC 1122
/// 4.2.2.13), plus 24 keyed bits so it stays unpredictable (RFC 6528).
const TIME_WAIT_GAP: u32 = 65537;
const OOO: usize = 4;
const EPHEMERAL: u32 = 49152;
const DEFAULT_MSS: u16 = 536;
/// The largest window scaling can express (RFC 7323).
const MAX_WINDOW: usize = 0xffff << 14;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Closed,
    Listen,
    SynSent,
    SynReceived,
    Established,
    FinWait1,
    FinWait2,
    CloseWait,
    Closing,
    LastAck,
    /// Only while a connection moves to the TIME_WAIT table; its slot then reads `Closed`.
    TimeWait,
}

/// A connection or listener: an index into the slots given to `Tcp::new`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TcpId(usize);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TcpInfo {
    pub state: State,
    pub error: Option<Error>,
    pub cwnd: u32,
    pub ssthresh: u32,
    pub snd_wnd: u32,
    pub rto: u64,
    /// When the connection's next timer fires: retransmission, persist, or the end of FIN-WAIT-2.
    pub deadline: Option<u64>,
    /// Bytes in the send ring, sent or not.
    pub queued: usize,
    /// The caller has released the connection (`tcp_close`); the stack finishes it on its own.
    pub released: bool,
}

/// A connection slot with its receive and send rings; a listener's receive ring sizes the window it offers.
pub struct TcpSocket<'a> {
    rx: &'a mut [u8],
    tx: &'a mut [u8],
    state: State,
    /// Held by the caller, or waiting in its listener's accept queue.
    open: bool,
    /// The listener of a connection not yet accepted.
    parent: Option<usize>,
    error: Option<Error>,
    /// A RST owed by `abort` or `tcp_close`, sent by the next `poll`.
    rst: bool,
    /// An ACK owed (a window update), sent by the next `poll`.
    ack_now: bool,
    /// Resend the first unacknowledged segment (fast retransmit, partial ACK).
    rexmit: bool,
    /// The persist timer fired: send what the window allows, or probe it.
    force: bool,
    /// The peer's SYN offered window scaling.
    scaled: bool,
    /// A FIN follows the queued data.
    shut: bool,
    peer_fin: bool,
    local: u16,
    remote: SocketAddrV4,
    mac: Mac,
    iss: u32,
    snd_una: u32,
    snd_nxt: u32,
    /// The highest sequence number sent, plus one; `snd_nxt` falls back to `snd_una` on a timeout.
    snd_max: u32,
    snd_wnd: u32,
    max_wnd: u32,
    wl1: u32,
    wl2: u32,
    snd_shift: u8,
    rcv_shift: u8,
    mss: u32,
    tx_head: usize,
    tx_len: usize,
    rcv_nxt: u32,
    /// The right edge of the window last advertised.
    rcv_adv: u32,
    rx_head: usize,
    rx_len: usize,
    /// Out-of-order data as offsets from `rcv_nxt`; empty when start == end.
    ooo: [(u32, u32); OOO],
    cwnd: u32,
    ssthresh: u32,
    /// Bytes acknowledged towards the next congestion-avoidance increase.
    acked: u32,
    dupacks: u8,
    recover: u32,
    recovery: bool,
    /// Slow start after a timeout grows by at most one MSS per ACK (RFC 3465 2.3).
    after_rto: bool,
    srtt: u64,
    rttvar: u64,
    rto: u64,
    /// When the running timer started: the last ACK of new data, a send with nothing outstanding, the last
    /// expiry, or (in FIN-WAIT-2) the peer's last segment. `deadline` derives the rest from the state.
    since: u64,
    /// `deadline()` as of the end of the last event that could change it.
    due: Option<u64>,
    retries: u8,
    probes: u8,
    /// Persist probes the peer has not answered, and all sent since the caller released the connection.
    unanswered: u8,
    orphan_probes: u8,
    /// A listener's last SYN cookie: cookie ACKs are checked only within two periods of it.
    cookie_at: Option<u64>,
    /// When an ACK last answered an out-of-window segment.
    oow_at: Option<u64>,
    /// The segment timed for an RTT sample (Karn: never a retransmission) and when it was sent.
    timed: Option<(u32, u64)>,
    /// When the current second of challenge ACKs started, and how many were sent in it.
    challenges: (u64, u8),
}

impl<'a> TcpSocket<'a> {
    pub fn new(rx: &'a mut [u8], tx: &'a mut [u8]) -> Self {
        TcpSocket {
            rx,
            tx,
            state: State::Closed,
            open: false,
            parent: None,
            error: None,
            rst: false,
            ack_now: false,
            rexmit: false,
            force: false,
            scaled: false,
            shut: false,
            peer_fin: false,
            local: 0,
            remote: SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0),
            mac: [0; 6],
            iss: 0,
            snd_una: 0,
            snd_nxt: 0,
            snd_max: 0,
            snd_wnd: 0,
            max_wnd: 0,
            wl1: 0,
            wl2: 0,
            snd_shift: 0,
            rcv_shift: 0,
            mss: DEFAULT_MSS as u32,
            tx_head: 0,
            tx_len: 0,
            rcv_nxt: 0,
            rcv_adv: 0,
            rx_head: 0,
            rx_len: 0,
            ooo: [(0, 0); OOO],
            cwnd: 0,
            ssthresh: u32::MAX,
            acked: 0,
            dupacks: 0,
            recover: 0,
            recovery: false,
            after_rto: false,
            srtt: 0,
            rttvar: 0,
            rto: INITIAL_RTO,
            since: 0,
            due: None,
            retries: 0,
            probes: 0,
            unanswered: 0,
            orphan_probes: 0,
            cookie_at: None,
            oow_at: None,
            timed: None,
            challenges: (0, 0),
        }
    }

    /// Clears the slot for a new connection, keeping its rings.
    fn reset(&mut self, state: State, local: u16, remote: SocketAddrV4) {
        let (rx, tx) = (core::mem::take(&mut self.rx), core::mem::take(&mut self.tx));
        *self = TcpSocket {
            state,
            open: true,
            local,
            remote,
            ..TcpSocket::new(rx, tx)
        };
    }

    fn free(&self) -> bool {
        self.state == State::Closed && !self.open && !self.rst
    }

    fn matches(&self, local: u16, remote: SocketAddrV4) -> bool {
        !matches!(self.state, State::Closed | State::Listen)
            && self.local == local
            && self.remote == remote
    }

    fn synchronized(&self) -> bool {
        !matches!(
            self.state,
            State::Closed | State::Listen | State::SynSent | State::SynReceived
        )
    }

    /// Free receive space; out-of-order data lives inside it.
    fn space(&self) -> u32 {
        (self.rx.len() - self.rx_len).min(MAX_WINDOW) as u32
    }

    fn window(&self) -> u16 {
        (self.space() >> self.rcv_shift).min(0xffff) as u16
    }

    fn data_end(&self) -> u32 {
        self.snd_una.wrapping_add(self.tx_len as u32)
    }

    /// A pure ACK; it carries `snd_max`, not a go-back-N `snd_nxt` the peer has already received past.
    fn ack_out(&mut self) -> Out {
        // Receiver silly-window avoidance (RFC 9293 3.8.6.2.2): the right edge moves by a whole step or not at all.
        let (full, held) = (self.space(), self.rcv_adv.wrapping_sub(self.rcv_nxt));
        let step = (self.rx.len() as u32 / 2).min(self.mss);
        let wnd = if held <= full && full - held < step {
            held
        } else {
            full
        };
        let win = (wnd >> self.rcv_shift).min(0xffff) as u16;
        self.rcv_adv = self.rcv_nxt.wrapping_add((win as u32) << self.rcv_shift);
        Out {
            seq: self.snd_max,
            ack: self.rcv_nxt,
            flags: ACK,
            win,
            syn: None,
        }
    }

    fn fail(&mut self, e: Error) {
        self.state = State::Closed;
        self.error = Some(e);
        (self.ack_now, self.rexmit, self.force) = (false, false, false);
    }

    fn challenge(&mut self, now: u64, out: &mut Option<Out>, c: &mut Counters) {
        if now.saturating_sub(self.challenges.0) >= SEC {
            self.challenges = (now, 0);
        }
        if self.challenges.1 < CHALLENGES {
            self.challenges.1 += 1;
            c.challenge_acks += 1;
            *out = Some(self.ack_out());
        }
    }

    fn establish(&mut self, now: u64) {
        self.state = if self.shut {
            State::FinWait1
        } else {
            State::Established
        };
        self.cwnd = initial_window(self.mss);
        self.rcv_adv = self
            .rcv_nxt
            .wrapping_add((self.window() as u32) << self.rcv_shift);
        (self.since, self.retries) = (now, 0);
    }

    /// Takes the SYN's MSS and window-scale options.
    fn options(&mut self, s: &Seg, mss: u16) {
        self.mss = s.mss.unwrap_or(DEFAULT_MSS).min(mss).max(64) as u32;
        (self.snd_shift, self.rcv_shift) = match s.shift {
            Some(peer) => (peer, shift_for(self.rx.len())),
            None => (0, 0),
        };
        self.scaled = s.shift.is_some();
    }

    /// One received segment on this connection; `out` gets the reply, if any.
    fn input(
        &mut self,
        s: &Seg,
        now: u64,
        mss: u16,
        out: &mut Option<Out>,
        c: &mut Counters,
    ) -> Result<(), Reason> {
        if self.state == State::SynSent {
            return self.syn_sent(s, now, mss, out);
        }
        if s.flags & SYN != 0 {
            self.challenge(now, out, c);
            return Err(Reason::Unacceptable);
        }
        let len = s.data.len() as u32 + (s.flags & FIN != 0) as u32;
        let wnd = self.space();
        let off = s.seq.wrapping_sub(self.rcv_nxt);
        // A segment at rcv_nxt into a zero window still carries a valid ACK (RFC 9293 3.10.7.4).
        let ok = match (len, wnd) {
            (_, 0) => off == 0,
            (0, _) => off < wnd,
            _ => off < wnd || off.wrapping_add(len - 1) < wnd,
        };
        if !ok {
            if s.flags & RST == 0 && self.oow_at.is_none_or(|t| now.saturating_sub(t) >= OOW_ACK) {
                self.oow_at = Some(now);
                *out = Some(self.ack_out());
            }
            return Err(Reason::Unacceptable);
        }
        if s.flags & RST != 0 {
            if off != 0 {
                self.challenge(now, out, c);
                return Err(Reason::Unacceptable);
            }
            self.fail(if self.state == State::SynReceived {
                Error::Refused
            } else {
                Error::Reset
            });
            return Ok(());
        }
        if s.flags & ACK == 0 {
            return Err(Reason::Unacceptable);
        }
        if self.state == State::SynReceived {
            if !(lt(self.snd_una, s.ack) && le(s.ack, self.snd_max)) {
                *out = Some(rst_for(s));
                return Err(Reason::Unacceptable);
            }
            self.snd_una = self.iss.wrapping_add(1);
            self.establish(now);
        }
        if gt(s.ack, self.snd_max) || lt(s.ack, self.snd_una.wrapping_sub(self.max_wnd)) {
            self.challenge(now, out, c);
            return Err(Reason::Unacceptable);
        }
        self.ack_in(s, now);
        if matches!(self.state, State::Closed | State::TimeWait) {
            return Ok(());
        }
        let receiving = matches!(
            self.state,
            State::Established | State::FinWait1 | State::FinWait2
        );
        if !s.data.is_empty() && receiving {
            if !self.open {
                // Data for a connection the caller closed cannot be delivered (RFC 9293 3.10.7.4, RFC 2525).
                *out = Some(Out {
                    seq: self.snd_max,
                    ack: 0,
                    flags: RST,
                    win: 0,
                    syn: None,
                });
                self.state = State::Closed;
                return Ok(());
            }
            self.store(s.seq, s.data);
        }
        let end = s.seq.wrapping_add(s.data.len() as u32);
        if s.flags & FIN != 0 && receiving && end == self.rcv_nxt {
            self.rcv_nxt = self.rcv_nxt.wrapping_add(1);
            self.peer_fin = true;
            self.state = match self.state {
                State::Established => State::CloseWait,
                State::FinWait1 => State::Closing,
                _ => State::TimeWait,
            };
        }
        if len > 0 {
            *out = Some(self.ack_out());
        }
        Ok(())
    }

    fn syn_sent(
        &mut self,
        s: &Seg,
        now: u64,
        mss: u16,
        out: &mut Option<Out>,
    ) -> Result<(), Reason> {
        let ack = s.flags & ACK != 0;
        if ack && (le(s.ack, self.iss) || gt(s.ack, self.snd_max)) {
            if s.flags & RST == 0 {
                *out = Some(rst_for(s));
            }
            return Err(Reason::Unacceptable);
        }
        if s.flags & RST != 0 {
            if !ack {
                return Err(Reason::Unacceptable);
            }
            self.fail(Error::Refused);
            return Ok(());
        }
        if s.flags & SYN == 0 {
            return Err(Reason::Unacceptable);
        }
        self.rcv_nxt = s.seq.wrapping_add(1);
        self.options(s, mss);
        (self.snd_wnd, self.max_wnd) = (s.win as u32, s.win as u32);
        (self.wl1, self.wl2) = (s.seq, s.ack);
        if ack {
            self.sample(s.ack, now);
            self.snd_una = s.ack;
            self.establish(now);
            *out = Some(self.ack_out());
        } else {
            // Simultaneous open: resend our SYN as a SYN-ACK.
            self.state = State::SynReceived;
            self.snd_nxt = self.iss;
        }
        Ok(())
    }

    fn ack_in(&mut self, s: &Seg, now: u64) {
        let win = (s.win as u32) << self.snd_shift;
        let changed = win != self.snd_wnd;
        self.unanswered = 0;
        let fresh = lt(self.wl1, s.seq) || (self.wl1 == s.seq && le(self.wl2, s.ack));
        if le(self.snd_una, s.ack) && fresh {
            if changed {
                self.probes = 0;
                if self.snd_una == self.snd_max && self.tx_len > 0 {
                    self.since = now;
                }
            }
            (self.snd_wnd, self.wl1, self.wl2) = (win, s.seq, s.ack);
            self.max_wnd = self.max_wnd.max(win);
        }
        let acked = s.ack.wrapping_sub(self.snd_una);
        if acked != 0 && le(s.ack, self.snd_max) && lt(self.snd_una, s.ack) {
            self.sample(s.ack, now);
            let fin_acked = self.shut && s.ack == self.data_end().wrapping_add(1);
            let bytes = (acked - fin_acked as u32) as usize;
            if bytes > 0 {
                self.tx_head = (self.tx_head + bytes) % self.tx.len();
                self.tx_len -= bytes;
            }
            self.snd_una = s.ack;
            if lt(self.snd_nxt, self.snd_una) {
                self.snd_nxt = self.snd_una;
            }
            self.grow(acked);
            (self.retries, self.probes, self.since) = (0, 0, now);
            if fin_acked {
                self.state = match self.state {
                    State::FinWait1 => State::FinWait2,
                    State::Closing => State::TimeWait,
                    State::LastAck => State::Closed,
                    state => state,
                };
            }
        } else if acked == 0
            && s.data.is_empty()
            && s.flags & FIN == 0
            && !changed
            && self.snd_una != self.snd_max
        {
            self.dupacks = self.dupacks.saturating_add(1);
            if self.recovery {
                self.cwnd = self.cwnd.saturating_add(self.mss);
            } else if self.dupacks == 3 && gt(s.ack, self.recover) {
                let flight = self.snd_max.wrapping_sub(self.snd_una);
                self.ssthresh = (flight / 2).max(2 * self.mss);
                self.cwnd = self.ssthresh + 3 * self.mss;
                (self.recover, self.recovery, self.rexmit) = (self.snd_max, true, true);
                self.timed = None;
            }
        }
    }

    /// NewReno on an ACK of new data: deflation and exit of fast recovery (RFC 6582), else byte counting.
    fn grow(&mut self, acked: u32) {
        let mss = self.mss;
        if self.recovery {
            if le(self.recover, self.snd_una) {
                let flight = self.snd_max.wrapping_sub(self.snd_una);
                self.cwnd = self.ssthresh.min(flight.max(mss) + mss);
                self.recovery = false;
            } else {
                self.rexmit = true;
                self.cwnd =
                    self.cwnd.saturating_sub(acked).max(mss) + if acked >= mss { mss } else { 0 };
            }
        } else if self.cwnd < self.ssthresh {
            let limit = if self.after_rto { mss } else { 2 * mss };
            self.cwnd = self.cwnd.saturating_add(acked.min(limit));
        } else {
            self.acked += acked;
            if self.acked >= self.cwnd {
                self.acked -= self.cwnd;
                self.cwnd += mss;
            }
        }
        self.cwnd = self.cwnd.min(MAX_WINDOW as u32);
        self.dupacks = 0;
        if self.after_rto && le(self.recover, self.snd_una) {
            self.after_rto = false;
        }
    }

    /// RFC 6298 2.2-2.4, for an ACK covering the timed segment.
    fn sample(&mut self, ack: u32, now: u64) {
        let Some((seq, at)) = self.timed else { return };
        if !gt(ack, seq) {
            return;
        }
        self.timed = None;
        let r = now.saturating_sub(at);
        if self.srtt == 0 {
            (self.srtt, self.rttvar) = (r, r / 2);
        } else {
            self.rttvar = (3 * self.rttvar + self.srtt.abs_diff(r)) / 4;
            self.srtt = (7 * self.srtt + r) / 8;
        }
        self.rto = (self.srtt + (4 * self.rttvar).max(MS)).clamp(MIN_RTO, MAX_RTO);
    }

    /// Writes in-window data at its offset and moves `rcv_nxt` over whatever is now in order.
    fn store(&mut self, seq: u32, data: &[u8]) {
        let off = seq.wrapping_sub(self.rcv_nxt) as i32;
        let (off, data) = if off < 0 {
            (0, data.get(off.unsigned_abs() as usize..).unwrap_or(&[]))
        } else {
            (off as u32, data)
        };
        let n = data.len().min(self.space().saturating_sub(off) as usize);
        if n == 0 {
            return;
        }
        let pos = (self.rx_head + self.rx_len + off as usize) % self.rx.len();
        ring_put(self.rx, pos, &data[..n]);
        let (mut a, mut b) = (off, off + n as u32);
        // Stored ranges never touch each other, so one pass merges everything this one touches.
        for r in &mut self.ooo {
            if r.0 < r.1 && r.0 <= b && a <= r.1 {
                (a, b) = (a.min(r.0), b.max(r.1));
                *r = (0, 0);
            }
        }
        if a > 0 {
            if let Some(r) = self.ooo.iter_mut().find(|r| r.0 == r.1) {
                *r = (a, b);
            }
            return;
        }
        for r in &mut self.ooo {
            if r.0 < r.1 {
                (r.0, r.1) = (r.0 - b, r.1 - b);
            }
        }
        self.rcv_nxt = self.rcv_nxt.wrapping_add(b);
        self.rx_len += b as usize;
    }

    /// Zero-window or blocked data with nothing else outstanding: the persist timer runs, not retransmission.
    fn persisting(&self) -> bool {
        self.synchronized()
            && self.tx_len > 0
            && (self.snd_wnd == 0 || self.snd_una == self.snd_max)
    }

    /// The connection's one deadline, derived from its state: FIN-WAIT-2's idle limit, the persist timer,
    /// retransmission (including a SYN not yet sent), or none for an idle or closed connection.
    fn deadline(&self) -> Option<u64> {
        let wait = match self.state {
            State::Closed | State::Listen | State::TimeWait => return None,
            // Linux's rule: only a released connection times out waiting for the peer's FIN.
            State::FinWait2 if !self.open => FIN_WAIT_2,
            _ if self.persisting() => (self.rto << self.probes.min(16)).min(MAX_RTO),
            _ if self.snd_una != self.snd_max || !self.synchronized() || self.fin_after(0) => {
                self.rto
            }
            _ => return None,
        };
        Some(self.since + wait)
    }

    /// Fires the deadline if it has come; true if it had, so the caller recomputes it.
    fn on_timer(&mut self, now: u64) -> bool {
        if self.due.is_none_or(|d| now < d) {
            return false;
        }
        if self.deadline().is_none_or(|d| now < d) {
            return true;
        }
        self.since = now;
        if self.state == State::FinWait2 {
            self.fail(Error::TimedOut);
        } else if self.persisting() {
            self.unanswered += 1;
            if self.unanswered > DATA_TRIES {
                self.fail(Error::TimedOut);
                return true;
            }
            self.orphan_probes += !self.open as u8;
            if self.orphan_probes > ORPHAN_PROBES {
                self.fail(Error::TimedOut);
                return true;
            }
            self.probes = self.probes.saturating_add(1);
            self.force = true;
        } else {
            self.retries += 1;
            let tries = if self.synchronized() {
                DATA_TRIES
            } else {
                SYN_TRIES
            };
            if self.retries > tries {
                self.fail(Error::TimedOut);
                return true;
            }
            if self.synchronized() {
                let flight = self.snd_max.wrapping_sub(self.snd_una);
                self.ssthresh = (flight / 2).max(2 * self.mss);
                (self.cwnd, self.after_rto) = (self.mss, true);
            }
            (self.recover, self.recovery, self.dupacks, self.acked) = (self.snd_max, false, 0, 0);
            (self.snd_nxt, self.timed, self.rexmit) = (self.snd_una, None, false);
            self.rto = (self.rto * 2).min(MAX_RTO);
        }
        true
    }

    /// Whether the next `poll` has something to send, so the next hop is worth resolving.
    fn wants_output(&self) -> bool {
        match self.state {
            State::SynSent | State::SynReceived => self.snd_nxt == self.iss || self.ack_now,
            State::Closed | State::Listen | State::TimeWait => false,
            _ => {
                self.ack_now
                    || self.rexmit
                    || self.force
                    || self.snd_nxt.wrapping_sub(self.snd_una) < self.tx_len as u32
                    || self.fin_after(0)
            }
        }
    }

    /// Whether a FIN goes out after the next `n` bytes: the send side is shut and they are the last.
    fn fin_after(&self, n: usize) -> bool {
        self.shut
            && matches!(
                self.state,
                State::FinWait1 | State::Closing | State::LastAck
            )
            && self.snd_nxt.wrapping_add(n as u32) == self.data_end()
    }

    /// The next segment to send, as its header and the length of data that follows it from `snd_una + offset`.
    fn next(&mut self, now: u64, mss: u16) -> Option<(Out, usize)> {
        if matches!(self.state, State::SynSent | State::SynReceived) {
            if self.snd_nxt != self.iss {
                return self.ack_now.then(|| {
                    self.ack_now = false;
                    (self.ack_out(), 0)
                });
            }
            let synack = self.state == State::SynReceived;
            let o = Out {
                seq: self.iss,
                ack: if synack { self.rcv_nxt } else { 0 },
                flags: if synack { SYN | ACK } else { SYN },
                win: self.rx.len().min(0xffff) as u16,
                syn: Some((
                    mss,
                    (!synack || self.scaled).then(|| shift_for(self.rx.len())),
                )),
            };
            self.sent(now, 1);
            return Some((o, 0));
        }
        let n = if self.rexmit {
            self.rexmit = false;
            let inflight = self.snd_max.wrapping_sub(self.snd_una) as usize;
            let n = inflight.min(self.tx_len).min(self.mss as usize);
            let fin = self.shut && inflight == self.tx_len + 1 && n == self.tx_len;
            if n > 0 || fin {
                let mut o = self.ack_out();
                o.seq = self.snd_una;
                o.flags |= if fin { FIN } else { PSH };
                return Some((o, n));
            }
            0
        } else {
            let inflight = self.snd_nxt.wrapping_sub(self.snd_una);
            let unsent = self.tx_len.saturating_sub(inflight as usize);
            let wnd = self.cwnd.min(self.snd_wnd);
            let usable = wnd.saturating_sub(inflight) as usize;
            let mut n = unsent.min(self.mss as usize).min(usable);
            if self.force {
                self.force = false;
                if n == 0 && self.tx_len > 0 {
                    // A zero-window probe: an old sequence number the peer must answer with its window.
                    let mut o = self.ack_out();
                    o.seq = self.snd_una.wrapping_sub(1);
                    return Some((o, 0));
                }
            } else if n < self.mss as usize
                && n < unsent
                && (n as u32) < self.max_wnd / 2
                && self.snd_nxt == self.snd_max
            {
                // Sender silly-window avoidance (RFC 9293 3.8.6.2.1), for new data only.
                n = 0;
            }
            n
        };
        let fin = self.fin_after(n);
        if n == 0 && !fin {
            return self.ack_now.then(|| {
                self.ack_now = false;
                (self.ack_out(), 0)
            });
        }
        let mut o = self.ack_out();
        o.seq = self.snd_nxt;
        o.flags |= if fin { FIN } else { 0 } | if n > 0 { PSH } else { 0 };
        self.sent(now, n as u32 + fin as u32);
        Some((o, n))
    }

    /// Advances `snd_nxt` over a segment of `len` sequence numbers just sent from it.
    fn sent(&mut self, now: u64, len: u32) {
        if self.snd_una == self.snd_max {
            self.since = now;
        }
        let seq = self.snd_nxt;
        self.snd_nxt = seq.wrapping_add(len);
        if gt(self.snd_nxt, self.snd_max) {
            if self.timed.is_none() && seq == self.snd_max {
                self.timed = Some((seq, now));
            }
            self.snd_max = self.snd_nxt;
        }
        self.ack_now = false;
    }
}

/// A passive open waiting for the handshake's last ACK; the caller fills the table with `HalfOpen::EMPTY`.
#[derive(Clone, Copy)]
pub struct HalfOpen {
    local: u16,
    /// Port 0 when the entry is free.
    remote: SocketAddrV4,
    mac: Mac,
    iss: u32,
    irs: u32,
    mss: u16,
    peer_shift: Option<u8>,
    shift: u8,
    win: u16,
    born: u64,
    tries: u8,
}

impl HalfOpen {
    pub const EMPTY: Self = HalfOpen {
        local: 0,
        remote: SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0),
        mac: [0; 6],
        iss: 0,
        irs: 0,
        mss: 0,
        peer_shift: None,
        shift: 0,
        win: 0,
        born: 0,
        tries: 0,
    };

    fn synack(&self, mss: u16) -> Out {
        Out {
            seq: self.iss,
            ack: self.irs.wrapping_add(1),
            flags: SYN | ACK,
            win: self.win,
            syn: Some((mss, self.peer_shift.map(|_| self.shift))),
        }
    }

    /// The next SYN-ACK retransmission: 1, 3, 7, ... s after the SYN.
    fn due(&self) -> u64 {
        self.born + SEC * ((2 << self.tries) - 1)
    }
}

/// A connection in TIME_WAIT; the caller fills the table with `TimeWait::EMPTY`.
#[derive(Clone, Copy)]
pub struct TimeWait {
    local: u16,
    remote: SocketAddrV4,
    mac: Mac,
    snd_nxt: u32,
    rcv_nxt: u32,
    /// Free once `now` reaches it.
    until: u64,
}

impl TimeWait {
    pub const EMPTY: Self = TimeWait {
        local: 0,
        remote: SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0),
        mac: [0; 6],
        snd_nxt: 0,
        rcv_nxt: 0,
        until: 0,
    };
}

/// One key per use, each SipHash of the caller's seed and a label, so no use's output (least of all the
/// half-open table's weak mix) tells anything about another's key.
struct Keys {
    isn: [u64; 2],
    port: [u64; 2],
    cookie: [u64; 2],
    time_wait: [u64; 2],
    slots: [u64; 2],
}

impl Keys {
    fn derive(seed: [u64; 2]) -> Self {
        let key = |label: u64| [siphash(seed, [label, 0]), siphash(seed, [label, 1])];
        Keys {
            isn: key(1),
            port: key(2),
            cookie: key(3),
            time_wait: key(4),
            slots: key(5),
        }
    }
}

/// TCP's memory and secret: connection slots, the half-open and TIME_WAIT tables, and the seed every key comes from.
pub struct Tcp<'a> {
    keys: Keys,
    sockets: &'a mut [TcpSocket<'a>],
    half_open: &'a mut [HalfOpen],
    time_wait: &'a mut [TimeWait],
    next_port: u32,
    /// The slot the last segment matched, tried first.
    last: usize,
    /// No half-open entry is due before this, so `poll` skips the table walk until then.
    half_open_due: Option<u64>,
}

impl<'a> Tcp<'a> {
    pub fn new(
        seed: [u64; 2],
        sockets: &'a mut [TcpSocket<'a>],
        half_open: &'a mut [HalfOpen],
        time_wait: &'a mut [TimeWait],
    ) -> Self {
        Tcp {
            keys: Keys::derive(seed),
            sockets,
            half_open,
            time_wait,
            next_port: 0,
            last: 0,
            half_open_due: None,
        }
    }
}

/// A received segment, its checksum and header checked.
struct Seg<'f> {
    from: SocketAddrV4,
    port: u16,
    seq: u32,
    ack: u32,
    flags: u8,
    win: u16,
    data: &'f [u8],
    mss: Option<u16>,
    shift: Option<u8>,
}

/// A segment header to send; `syn` carries the MSS and window-shift options.
#[derive(Clone, Copy)]
struct Out {
    seq: u32,
    ack: u32,
    flags: u8,
    win: u16,
    syn: Option<(u16, Option<u8>)>,
}

impl<'a> Stack<'a> {
    /// Gives the stack TCP; without it every TCP segment is answered with a RST.
    pub fn with_tcp(mut self, tcp: Tcp<'a>) -> Self {
        self.tcp = tcp;
        self
    }

    pub fn listen(&mut self, port: u16) -> Result<TcpId, Error> {
        if port == 0 {
            return Err(Error::Invalid);
        }
        if self.listener(port).is_some() {
            return Err(Error::InUse);
        }
        let i = self.free_slot()?;
        self.tcp.sockets[i].reset(
            State::Listen,
            port,
            SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0),
        );
        Ok(TcpId(i))
    }

    /// The next established connection on `listener`, if any.
    pub fn accept(&mut self, listener: TcpId) -> Option<TcpId> {
        let i = self
            .tcp
            .sockets
            .iter()
            .position(|s| s.parent == Some(listener.0))?;
        self.tcp.sockets[i].parent = None;
        Some(TcpId(i))
    }

    /// Opens a connection from `local` (0 picks an ephemeral port); the SYN goes out with the next `poll`.
    pub fn connect(&mut self, now: u64, local: u16, to: SocketAddrV4) -> Result<TcpId, Error> {
        if to.port() == 0 || !unicast(*to.ip()) {
            return Err(Error::Invalid);
        }
        self.next_hop(*to.ip())?;
        let local = match local {
            0 => self.ephemeral(to, now)?,
            port if self.in_use(port, to, now) => return Err(Error::InUse),
            port => port,
        };
        let i = self.free_slot()?;
        let iss = self.isn(local, to, now);
        let s = &mut self.tcp.sockets[i];
        s.reset(State::SynSent, local, to);
        (s.iss, s.snd_una, s.snd_nxt, s.snd_max, s.recover) = (iss, iss, iss, iss, iss);
        s.since = now;
        Ok(TcpId(i))
    }

    /// Queues as much of `data` as fits in the send ring.
    pub fn send(&mut self, id: TcpId, data: &[u8]) -> Result<usize, Error> {
        let s = self.tcp.sockets.get_mut(id.0).ok_or(Error::Closed)?;
        if let Some(e) = s.error {
            return Err(e);
        }
        let open = matches!(
            s.state,
            State::SynSent | State::SynReceived | State::Established | State::CloseWait
        );
        if !open || s.shut {
            return Err(Error::Closed);
        }
        let n = data.len().min(s.tx.len() - s.tx_len);
        if n == 0 {
            return if data.is_empty() {
                Ok(0)
            } else {
                Err(Error::WouldBlock)
            };
        }
        let pos = (s.tx_head + s.tx_len) % s.tx.len();
        ring_put(s.tx, pos, &data[..n]);
        s.tx_len += n;
        Ok(n)
    }

    /// Reads received data; `Ok(0)` is the end of the stream.
    pub fn recv(&mut self, id: TcpId, buf: &mut [u8]) -> Result<usize, Error> {
        let s = self.tcp.sockets.get_mut(id.0).ok_or(Error::Closed)?;
        let n = buf.len().min(s.rx_len);
        if n > 0 {
            ring_read(s.rx, s.rx_head, &mut buf[..n]);
            s.rx_head = (s.rx_head + n) % s.rx.len();
            s.rx_len -= n;
            return Ok(n);
        }
        if let Some(e) = s.error {
            return Err(e);
        }
        match s.state {
            _ if s.peer_fin || buf.is_empty() => Ok(0),
            State::Closed | State::Listen => Err(Error::Closed),
            _ => Err(Error::WouldBlock),
        }
    }

    /// Ends the send side: a FIN follows the queued data. Receiving goes on.
    pub fn shutdown(&mut self, id: TcpId) {
        let Some(s) = self.tcp.sockets.get_mut(id.0) else {
            return;
        };
        match s.state {
            State::SynSent | State::SynReceived => s.shut = true,
            State::Established => (s.state, s.shut) = (State::FinWait1, true),
            State::CloseWait => (s.state, s.shut) = (State::LastAck, true),
            _ => {}
        }
    }

    /// Releases the connection: it closes gracefully on its own, or is reset if received data was left unread.
    /// Closing a listener resets the connections it has not handed out.
    pub fn tcp_close(&mut self, id: TcpId) {
        let Some(s) = self.tcp.sockets.get_mut(id.0) else {
            return;
        };
        if s.state == State::Listen {
            s.state = State::Closed;
            s.open = false;
            for i in 0..self.tcp.sockets.len() {
                if self.tcp.sockets[i].parent == Some(id.0) {
                    self.abort(TcpId(i));
                }
            }
            return;
        }
        if s.rx_len > 0 {
            return self.abort(id);
        }
        if s.state == State::SynSent {
            s.state = State::Closed;
        }
        s.open = false;
        s.parent = None;
        s.due = s.deadline();
        self.shutdown(id);
    }

    /// Resets the connection and releases its slot.
    pub fn abort(&mut self, id: TcpId) {
        let Some(s) = self.tcp.sockets.get_mut(id.0) else {
            return;
        };
        s.rst = s.synchronized() || s.state == State::SynReceived;
        s.state = State::Closed;
        (s.open, s.parent) = (false, None);
    }

    pub fn tcp_info(&self, id: TcpId) -> Option<TcpInfo> {
        let s = self.tcp.sockets.get(id.0)?;
        Some(TcpInfo {
            state: s.state,
            error: s.error,
            cwnd: s.cwnd,
            ssthresh: s.ssthresh,
            snd_wnd: s.snd_wnd,
            rto: s.rto,
            deadline: s.due,
            queued: s.tx_len,
            released: !s.open,
        })
    }

    pub(crate) fn tcp_in(
        &mut self,
        eth: &[u8],
        src: Ipv4Addr,
        seg: &[u8],
        ours: Mac,
        now: u64,
        mss: u16,
    ) -> Result<(), Reason> {
        let s = parse(src, self.config.ip, seg)?;
        let mac: Mac = eth[6..12].try_into().unwrap();
        if !unicast_mac(mac) {
            return Err(Reason::Malformed);
        }
        let r = self.demux(&s, mac, ours, now, mss);
        if r.is_ok() {
            self.counters.tcp += 1;
        }
        r
    }

    fn demux(&mut self, s: &Seg, mac: Mac, ours: Mac, now: u64, mss: u16) -> Result<(), Reason> {
        let hint = self.tcp.last;
        let found = if self
            .tcp
            .sockets
            .get(hint)
            .is_some_and(|c| c.matches(s.port, s.from))
        {
            Some(hint)
        } else {
            self.tcp
                .sockets
                .iter()
                .position(|c| c.matches(s.port, s.from))
        };
        if let Some(i) = found {
            self.tcp.last = i;
            return self.conn_in(i, s, ours, now, mss);
        }
        let mut iss = None;
        if let Some(t) = self
            .tcp
            .time_wait
            .iter()
            .position(|t| t.until > now && t.local == s.port && t.remote == s.from)
        {
            let w = self.tcp.time_wait[t];
            // A new incarnation needs a listener and room for its gap ISS; else TIME_WAIT answers the SYN.
            let room = self.listener(s.port).is_some()
                && self
                    .slots(s.port, s.from)
                    .any(|h| self.tcp.half_open[h].remote.port() == 0);
            if s.flags & SYN != 0 && gt(s.seq, w.rcv_nxt) && room {
                self.tcp.time_wait[t] = TimeWait::EMPTY;
                let ips = (u32::from(self.config.ip) as u64) << 32 | u32::from(*s.from.ip()) as u64;
                let ports = (s.port as u64) << 16 | s.from.port() as u64;
                let keyed = siphash(self.tcp.keys.time_wait, [ips, ports, w.snd_nxt as u64]) as u32
                    & 0xff_ffff;
                iss = Some(w.snd_nxt.wrapping_add(TIME_WAIT_GAP + keyed));
            } else if s.flags & RST != 0 {
                // RFC 1337: a RST never cuts TIME_WAIT short.
                return Err(Reason::Unacceptable);
            } else {
                let fin_end = s.seq.wrapping_add(s.data.len() as u32 + 1);
                if s.flags & FIN != 0 && fin_end == w.rcv_nxt {
                    self.tcp.time_wait[t].until = now + TIME_WAIT;
                }
                let o = Out {
                    seq: w.snd_nxt,
                    ack: w.rcv_nxt,
                    flags: ACK,
                    win: 0,
                    syn: None,
                };
                self.reply_tcp((w.mac, ours), s.from, s.port, &o);
                return Ok(());
            }
        }
        let slots = self.slots(s.port, s.from);
        let table = &self.tcp.half_open;
        if let Some(h) = slots.clone().find(|&h| {
            table[h].remote.port() != 0 && table[h].local == s.port && table[h].remote == s.from
        }) {
            return self.half_open_in(h, s, ours, now, mss);
        }
        if let Some(l) = self.listener(s.port) {
            return self.listen_in(l, s, (mac, ours), now, mss, (iss, slots));
        }
        if s.flags & RST == 0 {
            self.reply_tcp((mac, ours), s.from, s.port, &rst_for(s));
        }
        Err(Reason::NoSocket)
    }

    fn conn_in(&mut self, i: usize, s: &Seg, ours: Mac, now: u64, mss: u16) -> Result<(), Reason> {
        let mut out = None;
        let c = &mut self.tcp.sockets[i];
        let r = c.input(s, now, mss, &mut out, &mut self.counters);
        let (mac, to, local) = (c.mac, c.remote, c.local);
        if let Some(o) = out {
            self.reply_tcp((mac, ours), to, local, &o);
        }
        self.settle(i, now);
        let c = &mut self.tcp.sockets[i];
        c.due = c.deadline();
        r
    }

    fn listen_in(
        &mut self,
        l: usize,
        s: &Seg,
        macs: (Mac, Mac),
        now: u64,
        mss: u16,
        (iss, mut slots): (Option<u32>, impl Iterator<Item = usize>),
    ) -> Result<(), Reason> {
        if s.flags & RST != 0 {
            return Err(Reason::Unacceptable);
        }
        if s.flags & ACK != 0 {
            let cookie = (s.flags & SYN == 0).then(|| self.cookie_in(l, s, macs.0, now));
            let Some(e) = cookie.flatten() else {
                self.counters.bad_cookies += 1;
                self.reply_tcp(macs, s.from, s.port, &rst_for(s));
                return Err(Reason::Unacceptable);
            };
            return self.open(e, l, s, macs.1, now, mss);
        }
        if s.flags & SYN == 0 {
            return Err(Reason::Unacceptable);
        }
        let rx = self.free_slot().unwrap_or(l);
        let rx = self.tcp.sockets[rx].rx.len();
        let win = rx.min(0xffff) as u16;
        let Some(h) = slots.find(|&h| self.tcp.half_open[h].remote.port() == 0) else {
            // A full table answers statelessly: the ISS encodes the MSS and is checked when the ACK returns.
            self.counters.syn_cookies += 1;
            self.tcp.sockets[l].cookie_at = Some(now);
            let peer = s.mss.unwrap_or(DEFAULT_MSS);
            let idx = COOKIE_MSS.iter().rposition(|&m| m <= peer).unwrap_or(0);
            let o = Out {
                seq: self.cookie(s.port, s.from, s.seq, now / COOKIE_PERIOD, idx as u32),
                ack: s.seq.wrapping_add(1),
                flags: SYN | ACK,
                win,
                syn: Some((mss, None)),
            };
            self.reply_tcp(macs, s.from, s.port, &o);
            return Ok(());
        };
        let e = HalfOpen {
            local: s.port,
            remote: s.from,
            mac: macs.0,
            iss: iss.unwrap_or_else(|| self.isn(s.port, s.from, now)),
            irs: s.seq,
            mss: s.mss.unwrap_or(DEFAULT_MSS),
            peer_shift: s.shift,
            shift: shift_for(rx),
            win,
            born: now,
            tries: 0,
        };
        self.tcp.half_open[h] = e;
        self.tcp.half_open_due = crate::earliest(self.tcp.half_open_due, Some(e.due()));
        self.reply_tcp(macs, s.from, s.port, &e.synack(mss));
        Ok(())
    }

    /// The cookie ISS for a SYN: the clock's low bit, the 2-bit MSS index and 29 bits of keyed hash over the whole
    /// clock, the index, the connection and the peer's ISN (so a cookie two periods old fails, low bit or not).
    fn cookie(&self, local: u16, from: SocketAddrV4, irs: u32, t: u64, idx: u32) -> u32 {
        let ips = (u32::from(self.config.ip) as u64) << 32 | u32::from(*from.ip()) as u64;
        let ports = (local as u64) << 48 | (from.port() as u64) << 32 | irs as u64;
        let hash = siphash(self.tcp.keys.cookie, [ips, ports, t << 2 | idx as u64]) as u32;
        ((t as u32 & 1) << 31) | (idx << 29) | (hash & 0x1fff_ffff)
    }

    /// The half-open state a valid cookie ACK stands for: issued this period or the last, for this SYN, while
    /// listener `l` has sent cookies within that time (no flood on it, no cookie to guess).
    fn cookie_in(&self, l: usize, s: &Seg, mac: Mac, now: u64) -> Option<HalfOpen> {
        self.tcp.sockets[l]
            .cookie_at
            .filter(|&t| now.saturating_sub(t) < 2 * COOKIE_PERIOD)?;
        let (iss, irs) = (s.ack.wrapping_sub(1), s.seq.wrapping_sub(1));
        let idx = (iss >> 29) & 3;
        let t = [now / COOKIE_PERIOD, (now / COOKIE_PERIOD).saturating_sub(1)];
        t.iter()
            .any(|&t| self.cookie(s.port, s.from, irs, t, idx) == iss)
            .then_some(HalfOpen {
                local: s.port,
                remote: s.from,
                mac,
                iss,
                irs,
                mss: COOKIE_MSS[idx as usize],
                born: now,
                ..HalfOpen::EMPTY
            })
    }

    fn half_open_in(
        &mut self,
        h: usize,
        s: &Seg,
        ours: Mac,
        now: u64,
        mss: u16,
    ) -> Result<(), Reason> {
        let e = self.tcp.half_open[h];
        if s.flags & RST != 0 {
            if s.seq != e.irs.wrapping_add(1) {
                return Err(Reason::Unacceptable);
            }
            self.tcp.half_open[h] = HalfOpen::EMPTY;
            return Ok(());
        }
        if s.flags & SYN != 0 {
            if s.seq != e.irs {
                return Err(Reason::Unacceptable);
            }
            self.reply_tcp((e.mac, ours), s.from, s.port, &e.synack(mss));
            return Ok(());
        }
        if s.flags & ACK == 0 || s.seq != e.irs.wrapping_add(1) {
            return Err(Reason::Unacceptable);
        }
        let Some(l) = self.listener(e.local) else {
            self.tcp.half_open[h] = HalfOpen::EMPTY;
            self.reply_tcp((e.mac, ours), s.from, s.port, &rst_for(s));
            return Err(Reason::NoSocket);
        };
        if s.ack != e.iss.wrapping_add(1) {
            // The client may be answering a cookie SYN-ACK sent before this entry existed.
            if let Some(c) = self.cookie_in(l, s, e.mac, now) {
                self.open(c, l, s, ours, now, mss)?;
                self.tcp.half_open[h] = HalfOpen::EMPTY;
                return Ok(());
            }
            self.reply_tcp((e.mac, ours), s.from, s.port, &rst_for(s));
            return Err(Reason::Unacceptable);
        }
        self.open(e, l, s, ours, now, mss)?;
        self.tcp.half_open[h] = HalfOpen::EMPTY;
        Ok(())
    }

    /// Turns a completed handshake into a connection on listener `l`, then hands it the ACK segment.
    fn open(
        &mut self,
        e: HalfOpen,
        l: usize,
        s: &Seg,
        ours: Mac,
        now: u64,
        mss: u16,
    ) -> Result<(), Reason> {
        let i = self.free_slot().map_err(|_| Reason::SocketFull)?;
        let c = &mut self.tcp.sockets[i];
        c.reset(State::SynReceived, e.local, e.remote);
        let una = e.iss.wrapping_add(1);
        (c.parent, c.mac, c.iss, c.recover) = (Some(l), e.mac, e.iss, e.iss);
        (c.snd_una, c.snd_nxt, c.snd_max) = (una, una, una);
        (c.rcv_nxt, c.wl1, c.wl2) = (e.irs.wrapping_add(1), e.irs, una);
        c.mss = e.mss.min(mss).max(64) as u32;
        (c.snd_shift, c.rcv_shift) = match e.peer_shift {
            Some(peer) => (peer, e.shift),
            None => (0, 0),
        };
        c.establish(now);
        self.conn_in(i, s, ours, now, mss)
    }

    /// Checks an ICMP error's quoted segment against its connection (RFC 5927); `quote` follows the ICMP header.
    pub(crate) fn tcp_icmp(&mut self, quote: &[u8], hard: bool) -> Result<(), Reason> {
        let ihl = (*quote.first().ok_or(Reason::Malformed)? & 15) as usize * 4;
        let q = quote.get(..ihl + 8).ok_or(Reason::Malformed)?;
        if q[0] >> 4 != 4 || ihl < IP || q[9] != PROTO_TCP || ip_at(&q[12..16]) != self.config.ip {
            return Err(Reason::Ignored);
        }
        let t = &q[ihl..];
        let remote = SocketAddrV4::new(ip_at(&q[16..20]), be16(&t[2..4]));
        let local = be16(&t[..2]);
        let seq = u32::from_be_bytes([t[4], t[5], t[6], t[7]]);
        let c = self
            .tcp
            .sockets
            .iter_mut()
            .find(|c| c.matches(local, remote))
            .ok_or(Reason::Ignored)?;
        if !(le(c.snd_una, seq) && lt(seq, c.snd_max)) {
            return Err(Reason::Unacceptable);
        }
        if hard && c.state == State::SynSent {
            c.fail(Error::Unreachable);
        }
        self.counters.tcp += 1;
        Ok(())
    }

    /// Timers, retransmissions and queued data for every connection; returns the next TCP deadline.
    pub(crate) fn tcp_poll(&mut self, nic: &mut impl Nic, now: u64) -> Option<u64> {
        let (mss, ours) = (mss(nic), nic.mac());
        let mut next = None::<u64>;
        let walk = self.tcp.half_open_due.is_some_and(|t| now >= t);
        for h in (0..self.tcp.half_open.len()).filter(|_| walk) {
            let e = self.tcp.half_open[h];
            if e.remote.port() == 0 {
                continue;
            }
            if now >= e.due() {
                if e.tries >= SYNACK_TRIES {
                    self.tcp.half_open[h] = HalfOpen::EMPTY;
                    continue;
                }
                self.tcp.half_open[h].tries += 1;
                self.ip_id = self.ip_id.wrapping_add(1);
                let (src, id, o) = (self.config.ip, self.ip_id, e.synack(mss));
                let sent = nic.transmit(ETH + IP + TCP + SYN_OPTIONS, |f| {
                    frame(f, (e.mac, ours), (src, e.remote), e.local, id, &o, |_| {})
                });
                self.count_tx(sent);
            }
            next = crate::earliest(next, Some(self.tcp.half_open[h].due()));
        }
        if walk {
            self.tcp.half_open_due = next;
        }
        let mut next = self.tcp.half_open_due;
        for i in 0..self.tcp.sockets.len() {
            let fired = self.tcp.sockets[i].on_timer(now);
            if self.tcp_output(nic, now, i, mss) || fired {
                let s = &mut self.tcp.sockets[i];
                s.due = s.deadline();
            }
            next = crate::earliest(next, self.tcp.sockets[i].due);
        }
        next
    }

    /// Sends what the connection owes; true if it had anything to send, which may move its deadline.
    fn tcp_output(&mut self, nic: &mut impl Nic, now: u64, i: usize, mss: u16) -> bool {
        let s = &mut self.tcp.sockets[i];
        if matches!(
            s.state,
            State::Established | State::FinWait1 | State::FinWait2
        ) {
            let edge = s.rcv_nxt.wrapping_add((s.window() as u32) << s.rcv_shift);
            let step = (s.rx.len() as u32 / 2).min(s.mss);
            if edge.wrapping_sub(s.rcv_adv) as i32 >= step.max(1) as i32 {
                s.ack_now = true;
            }
        }
        let (src, ours, remote) = (self.config.ip, nic.mac(), s.remote);
        if core::mem::take(&mut s.rst) {
            let o = Out {
                seq: s.snd_max,
                ack: s.rcv_nxt,
                flags: RST | ACK,
                win: 0,
                syn: None,
            };
            self.ip_id = self.ip_id.wrapping_add(1);
            let (mac, local, id) = (s.mac, s.local, self.ip_id);
            let sent = nic.transmit(ETH + IP + TCP, |f| {
                frame(f, (mac, ours), (src, remote), local, id, &o, |_| {})
            });
            self.count_tx(sent);
            return true;
        }
        if !s.wants_output() {
            return false;
        }
        let Ok(mac) = self
            .next_hop(*remote.ip())
            .and_then(|ip| self.resolve(nic, ip, now))
        else {
            return true;
        };
        self.tcp.sockets[i].mac = mac;
        loop {
            let s = &mut self.tcp.sockets[i];
            let Some((o, n)) = s.next(now, mss) else {
                break;
            };
            self.ip_id = self.ip_id.wrapping_add(1);
            let id = self.ip_id;
            let hl = TCP + if o.syn.is_some() { SYN_OPTIONS } else { 0 };
            let pos = if n > 0 {
                (s.tx_head + o.seq.wrapping_sub(s.snd_una) as usize) % s.tx.len()
            } else {
                0
            };
            let (tx, local) = (&*s.tx, s.local);
            let sent = nic.transmit(ETH + IP + hl + n, |f| {
                frame(f, (mac, ours), (src, remote), local, id, &o, |p| {
                    ring_read(tx, pos, p)
                })
            });
            self.count_tx(sent);
            if !sent {
                break;
            }
        }
        true
    }

    /// Moves a connection that reached TIME_WAIT into the TIME_WAIT table.
    fn settle(&mut self, i: usize, now: u64) {
        let s = &mut self.tcp.sockets[i];
        if s.state != State::TimeWait {
            return;
        }
        s.state = State::Closed;
        let entry = TimeWait {
            local: s.local,
            remote: s.remote,
            mac: s.mac,
            snd_nxt: s.snd_max,
            rcv_nxt: s.rcv_nxt,
            until: now + TIME_WAIT,
        };
        let table = &mut self.tcp.time_wait;
        let Some(t) = (0..table.len()).min_by_key(|&t| table[t].until) else {
            return;
        };
        if table[t].until > now {
            self.counters.time_wait_reused += 1;
        }
        table[t] = entry;
    }

    fn reply_tcp(&mut self, macs: (Mac, Mac), to: SocketAddrV4, local: u16, o: &Out) {
        let len = ETH + IP + TCP + if o.syn.is_some() { SYN_OPTIONS } else { 0 };
        self.ip_id = self.ip_id.wrapping_add(1);
        let (src, id) = (self.config.ip, self.ip_id);
        frame(
            &mut self.reply[..len],
            macs,
            (src, to),
            local,
            id,
            o,
            |_| {},
        );
        self.reply_len = len;
    }

    /// The half-open slots a connection may occupy: `PROBES` in a row from a keyed hash, so a lookup is O(1).
    fn slots(&self, local: u16, from: SocketAddrV4) -> impl Iterator<Item = usize> + Clone + use<> {
        let n = self.tcp.half_open.len();
        let ips = (u32::from(self.config.ip) as u64) << 32 | u32::from(*from.ip()) as u64;
        let ports = (local as u64) << 16 | from.port() as u64;
        // A keyed mix, not SipHash: steering SYNs into one run only sends them to cookies.
        let k = self.tcp.keys.slots;
        let x = (ips ^ k[0]).wrapping_mul(0x9e37_79b9_7f4a_7c15) ^ (ports ^ k[1]);
        let h = (x.wrapping_mul(0xbf58_476d_1ce4_e5b9) >> 32) as usize;
        (0..PROBES.min(n)).map(move |i| (h + i) % n)
    }

    fn listener(&self, port: u16) -> Option<usize> {
        self.tcp
            .sockets
            .iter()
            .position(|s| s.state == State::Listen && s.local == port)
    }

    fn free_slot(&self) -> Result<usize, Error> {
        self.tcp
            .sockets
            .iter()
            .position(|s| s.free())
            .ok_or(Error::TableFull)
    }

    fn in_use(&self, local: u16, to: SocketAddrV4, now: u64) -> bool {
        self.tcp.sockets.iter().any(|s| s.matches(local, to))
            || self
                .tcp
                .time_wait
                .iter()
                .any(|t| t.until > now && t.local == local && t.remote == to)
    }

    /// RFC 6056 algorithm 3: a keyed offset per destination plus a counter.
    fn ephemeral(&mut self, to: SocketAddrV4, now: u64) -> Result<u16, Error> {
        let range = 65536 - EPHEMERAL;
        let ips = (u32::from(self.config.ip) as u64) << 32 | u32::from(*to.ip()) as u64;
        let offset = siphash(self.tcp.keys.port, [ips, to.port() as u64]) as u32;
        for n in 0..range {
            let port = (EPHEMERAL + offset.wrapping_add(self.tcp.next_port.wrapping_add(n)) % range)
                as u16;
            if !self.in_use(port, to, now) && self.listener(port).is_none() {
                self.tcp.next_port = self.tcp.next_port.wrapping_add(n + 1);
                return Ok(port);
            }
        }
        Err(Error::InUse)
    }

    /// RFC 6528: a keyed hash of the connection plus a 4 us clock.
    fn isn(&self, local: u16, to: SocketAddrV4, now: u64) -> u32 {
        let ips = (u32::from(self.config.ip) as u64) << 32 | u32::from(*to.ip()) as u64;
        let ports = (local as u64) << 16 | to.port() as u64;
        (siphash(self.tcp.keys.isn, [ips, ports]) as u32).wrapping_add((now / 4000) as u32)
    }
}

/// Our MSS on this NIC.
pub(crate) fn mss(nic: &impl Nic) -> u16 {
    (nic.mtu().min(MAX_PACKET) - IP - TCP) as u16
}

fn parse<'f>(src: Ipv4Addr, dst: Ipv4Addr, seg: &'f [u8]) -> Result<Seg<'f>, Reason> {
    let h = seg.get(..TCP).ok_or(Reason::Malformed)?;
    if fold(pseudo(src, dst, PROTO_TCP, seg.len() as u16) + sum(seg)) != 0xffff {
        return Err(Reason::Checksum);
    }
    let off = (h[12] >> 4) as usize * 4;
    let (sport, port, flags) = (be16(&h[..2]), be16(&h[2..4]), h[13]);
    if off < TCP
        || off > seg.len()
        || sport == 0
        || port == 0
        || (flags & SYN != 0 && flags & (RST | FIN) != 0)
    {
        return Err(Reason::Malformed);
    }
    let (mut mss, mut shift) = (None, None);
    if flags & SYN != 0 {
        let mut o = &seg[TCP..off];
        while let [kind, rest @ ..] = o {
            match kind {
                0 => break,
                1 => o = rest,
                _ => {
                    let len = *rest.first().ok_or(Reason::Malformed)? as usize;
                    if len < 2 || len > o.len() {
                        return Err(Reason::Malformed);
                    }
                    match (kind, len) {
                        (2, 4) => mss = Some(be16(&o[2..4])),
                        (3, 3) => shift = Some(o[2].min(14)),
                        _ => {}
                    }
                    o = &o[len..];
                }
            }
        }
    }
    Ok(Seg {
        from: SocketAddrV4::new(src, sport),
        port,
        seq: u32::from_be_bytes([h[4], h[5], h[6], h[7]]),
        ack: u32::from_be_bytes([h[8], h[9], h[10], h[11]]),
        flags,
        win: be16(&h[14..16]),
        data: &seg[off..],
        mss,
        shift,
    })
}

/// The RST answering `s` when no connection takes it (RFC 9293 3.10.7.1).
fn rst_for(s: &Seg) -> Out {
    let len = s.data.len() as u32 + (s.flags & SYN != 0) as u32 + (s.flags & FIN != 0) as u32;
    if s.flags & ACK != 0 {
        Out {
            seq: s.ack,
            ack: 0,
            flags: RST,
            win: 0,
            syn: None,
        }
    } else {
        Out {
            seq: 0,
            ack: s.seq.wrapping_add(len),
            flags: RST | ACK,
            win: 0,
            syn: None,
        }
    }
}

/// Writes a whole frame: Ethernet, IPv4 and the TCP header of `o`, then `fill` writes the payload, then the checksum.
fn frame(
    f: &mut [u8],
    macs: (Mac, Mac),
    ips: (Ipv4Addr, SocketAddrV4),
    local: u16,
    id: u16,
    o: &Out,
    fill: impl FnOnce(&mut [u8]),
) {
    let (src, to) = ips;
    let total = f.len() - ETH;
    write_eth(f, macs.0, macs.1, TYPE_IPV4);
    write_ip(&mut f[ETH..], src, *to.ip(), PROTO_TCP, total, id);
    let seg = &mut f[ETH + IP..];
    let len = seg.len() as u16;
    let hl = TCP + if o.syn.is_some() { SYN_OPTIONS } else { 0 };
    let (h, payload) = seg.split_at_mut(hl);
    h[..2].copy_from_slice(&local.to_be_bytes());
    h[2..4].copy_from_slice(&to.port().to_be_bytes());
    h[4..8].copy_from_slice(&o.seq.to_be_bytes());
    h[8..12].copy_from_slice(&o.ack.to_be_bytes());
    h[12..14].copy_from_slice(&[(hl as u8 / 4) << 4, o.flags]);
    h[14..16].copy_from_slice(&o.win.to_be_bytes());
    h[16..20].fill(0);
    if let Some((mss, shift)) = o.syn {
        let [m0, m1] = mss.to_be_bytes();
        h[20..24].copy_from_slice(&[2, 4, m0, m1]);
        h[24..].copy_from_slice(&match shift {
            Some(s) => [1, 3, 3, s],
            None => [1, 1, 1, 1],
        });
    }
    fill(payload);
    let c = !fold(pseudo(src, *to.ip(), PROTO_TCP, len) + sum(h) + sum(payload)) as u16;
    h[16..18].copy_from_slice(&c.to_be_bytes());
}

/// RFC 5681 3.1.
fn initial_window(mss: u32) -> u32 {
    mss * if mss > 2190 {
        2
    } else if mss > 1095 {
        3
    } else {
        4
    }
}

/// The window shift that lets a `len`-byte ring be advertised in 16 bits.
fn shift_for(len: usize) -> u8 {
    (0..14).find(|&s| len >> s <= 0xffff).unwrap_or(14)
}

fn be16(b: &[u8]) -> u16 {
    u16::from_be_bytes([b[0], b[1]])
}

fn lt(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) < 0
}

fn le(a: u32, b: u32) -> bool {
    !lt(b, a)
}

fn gt(a: u32, b: u32) -> bool {
    lt(b, a)
}

/// SipHash-2-4 of a message given as little-endian words.
fn siphash<const N: usize>(key: [u64; 2], m: [u64; N]) -> u64 {
    let mut v = [
        key[0] ^ 0x736f_6d65_7073_6575,
        key[1] ^ 0x646f_7261_6e64_6f6d,
        key[0] ^ 0x6c79_6765_6e65_7261,
        key[1] ^ 0x7465_6462_7974_6573,
    ];
    let round = |v: &mut [u64; 4]| {
        v[0] = v[0].wrapping_add(v[1]);
        v[1] = v[1].rotate_left(13) ^ v[0];
        v[0] = v[0].rotate_left(32);
        v[2] = v[2].wrapping_add(v[3]);
        v[3] = v[3].rotate_left(16) ^ v[2];
        v[0] = v[0].wrapping_add(v[3]);
        v[3] = v[3].rotate_left(21) ^ v[0];
        v[2] = v[2].wrapping_add(v[1]);
        v[1] = v[1].rotate_left(17) ^ v[2];
        v[2] = v[2].rotate_left(32);
    };
    let mut eat = |w: u64| {
        v[3] ^= w;
        round(&mut v);
        round(&mut v);
        v[0] ^= w;
    };
    for w in m {
        eat(w);
    }
    eat((N as u64 * 8) << 56);
    v[2] ^= 0xff;
    for _ in 0..4 {
        round(&mut v);
    }
    v[0] ^ v[1] ^ v[2] ^ v[3]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[allow(deprecated)]
    fn siphash_matches_the_reference() {
        use std::hash::Hasher;
        let (key, m) = (
            [0x0706_0504_0302_0100, 0x0f0e_0d0c_0b0a_0908],
            [7u64, 0xdead_beef],
        );
        let mut h = std::hash::SipHasher::new_with_keys(key[0], key[1]);
        h.write(&m[0].to_le_bytes());
        h.write(&m[1].to_le_bytes());
        assert_eq!(siphash(key, m), h.finish());
    }

    #[test]
    #[allow(deprecated)]
    fn siphash_of_three_words_matches_the_reference() {
        use std::hash::Hasher;
        let (key, m) = ([3, 4], [7u64, 8, 9]);
        let mut h = std::hash::SipHasher::new_with_keys(key[0], key[1]);
        m.iter().for_each(|w| h.write(&w.to_le_bytes()));
        assert_eq!(siphash(key, m), h.finish());
    }

    #[test]
    fn each_use_gets_its_own_key_derived_from_the_seed() {
        let seed = [1, 2];
        let k = Tcp::new(seed, &mut [], &mut [], &mut []).keys;
        let all = [seed, k.isn, k.port, k.cookie, k.time_wait, k.slots];
        for (i, a) in all.iter().enumerate() {
            for b in &all[i + 1..] {
                assert_ne!(a, b, "two uses share a key");
            }
        }
        assert_ne!(
            k.slots, k.cookie,
            "the weak half-open mix never sees the cookie key"
        );
        let other = Tcp::new([1, 3], &mut [], &mut [], &mut []).keys;
        assert_ne!(other.cookie, k.cookie, "every key depends on the seed");
    }

    #[test]
    fn reassembly_merges_out_of_order_ranges() {
        let (mut rx, mut tx) = ([0u8; 64], [0u8; 0]);
        let mut s = TcpSocket::new(&mut rx, &mut tx);
        let base = u32::MAX - 4;
        s.rcv_nxt = base;
        let stream: Vec<u8> = (0..40).collect();
        for (from, to) in [
            (10, 20),
            (30, 35),
            (20, 25),
            (5, 8),
            (0, 6),
            (8, 10),
            (25, 40),
        ] {
            s.store(base.wrapping_add(from as u32), &stream[from..to]);
        }
        assert_eq!((s.rx_len, s.rcv_nxt), (40, 35));
        assert!(s.ooo.iter().all(|r| r.0 == r.1));
        assert_eq!(&s.rx[..40], &stream[..]);
    }
}
