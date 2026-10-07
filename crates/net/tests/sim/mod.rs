//! A simulated Ethernet link between two NICs in virtual time, with seeded faults so a failing seed replays exactly.
#![allow(dead_code)]

use std::cmp::Reverse;
use std::collections::BinaryHeap;

use net::{Mac, Nic};

/// Fault rates in parts per thousand, and the one-way delay in ns.
#[derive(Clone, Copy, Default)]
pub struct Faults {
    pub loss: u64,
    pub duplicate: u64,
    pub reorder: u64,
    pub corrupt: u64,
    pub delay: u64,
    /// Extra loss for ARP frames, so neighbours fail to resolve.
    pub arp_loss: u64,
}

pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1)
    }

    pub fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    pub fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    pub fn chance(&mut self, per_mille: u64) -> bool {
        self.below(1000) < per_mille
    }
}

/// A frame in flight: delivery time, send order, frame; the heap pops the smallest.
type InFlight = Reverse<(u64, u64, Vec<u8>)>;

pub struct Link {
    pub rng: Rng,
    pub faults: Faults,
    pub now: u64,
    macs: [Mac; 2],
    /// Frames in flight to each side, earliest delivery first, then in the order sent.
    queues: [BinaryHeap<InFlight>; 2],
    sent: u64,
    spare: Vec<Vec<u8>>,
    /// Every frame sent, before faults, when set.
    pub record: Option<Vec<Vec<u8>>>,
}

impl Link {
    pub fn new(seed: u64, faults: Faults, macs: [Mac; 2]) -> Self {
        Link {
            rng: Rng::new(seed),
            faults,
            now: 0,
            macs,
            queues: [BinaryHeap::new(), BinaryHeap::new()],
            sent: 0,
            spare: Vec::new(),
            record: None,
        }
    }

    /// When the next frame in flight arrives.
    pub fn next(&self) -> Option<u64> {
        self.queues
            .iter()
            .filter_map(|q| q.peek())
            .map(|q| q.0.0)
            .min()
    }

    /// The NIC on `side` (0 or 1).
    pub fn end(&mut self, side: usize) -> End<'_> {
        End { link: self, side }
    }
}

/// A NIC fed and drained by hand: `rx` frames are received in order, transmitted frames land in `tx`.
pub struct Tap {
    pub mac: Mac,
    pub rx: std::collections::VecDeque<Vec<u8>>,
    pub tx: Vec<Vec<u8>>,
}

impl Tap {
    pub fn new(mac: Mac) -> Self {
        Tap {
            mac,
            rx: Default::default(),
            tx: Vec::new(),
        }
    }
}

impl Nic for Tap {
    fn mac(&self) -> Mac {
        self.mac
    }

    fn mtu(&self) -> usize {
        1500
    }

    fn transmit(&mut self, len: usize, fill: impl FnOnce(&mut [u8])) -> bool {
        let mut frame = vec![0; len];
        fill(&mut frame);
        self.tx.push(frame);
        true
    }

    fn receive(&mut self, f: impl FnOnce(&[u8])) -> bool {
        self.rx.pop_front().map(|frame| f(&frame)).is_some()
    }
}

pub struct End<'l> {
    link: &'l mut Link,
    side: usize,
}

impl Nic for End<'_> {
    fn mac(&self) -> Mac {
        self.link.macs[self.side]
    }

    fn mtu(&self) -> usize {
        1500
    }

    fn transmit(&mut self, len: usize, fill: impl FnOnce(&mut [u8])) -> bool {
        let link = &mut *self.link;
        let mut frame = link.spare.pop().unwrap_or_default();
        frame.clear();
        frame.resize(len, 0);
        fill(&mut frame);
        if let Some(record) = &mut link.record {
            record.push(frame.clone());
        }
        let f = link.faults;
        let arp = frame.get(12..14) == Some(&[8, 6]);
        if link.rng.chance(f.loss) || (arp && link.rng.chance(f.arp_loss)) {
            link.spare.push(frame);
            return true;
        }
        if link.rng.chance(f.corrupt) {
            let bit = link.rng.below(len as u64 * 8) as usize;
            frame[bit / 8] ^= 1 << (bit % 8);
        }
        let mut at = link.now + f.delay;
        if link.rng.chance(f.reorder) {
            at += f.delay + link.rng.below(4 * f.delay + 1);
        }
        let queue = &mut link.queues[1 - self.side];
        if link.rng.chance(f.duplicate) {
            link.sent += 1;
            queue.push(Reverse((
                at + link.rng.below(f.delay + 1),
                link.sent,
                frame.clone(),
            )));
        }
        link.sent += 1;
        queue.push(Reverse((at, link.sent, frame)));
        true
    }

    fn receive(&mut self, f: impl FnOnce(&[u8])) -> bool {
        let link = &mut *self.link;
        let queue = &mut link.queues[self.side];
        if queue.peek().is_none_or(|q| q.0.0 > link.now) {
            return false;
        }
        let Reverse((_, _, frame)) = queue.pop().unwrap();
        f(&frame);
        link.spare.push(frame);
        true
    }
}

/// Applies one mutation; returns false if the Internet checksum may miss it (it cannot tell 0x0000 from 0xffff).
pub fn mutate(rng: &mut Rng, frame: &mut Vec<u8>) -> bool {
    let len = frame.len() as u64;
    match rng.below(6) {
        1 => frame[rng.below(len.min(64)) as usize] = rng.next() as u8,
        2 => frame[rng.below(len) as usize] = [0, 0xff][rng.below(2) as usize],
        3 => frame.truncate(rng.below(len) as usize),
        4 => frame.extend((0..rng.below(64)).map(|_| rng.next() as u8)),
        5 if len >= 2 => {
            let i = rng.below(len.min(48) / 2) as usize * 2;
            let v = [0u16, 1, 0x7fff, 0x8000, 0xffff][rng.below(5) as usize];
            frame[i..i + 2].copy_from_slice(&v.to_be_bytes());
            return false;
        }
        _ => {
            let bit = rng.below(len * 8) as usize;
            frame[bit / 8] ^= 1 << (bit % 8);
        }
    }
    true
}
