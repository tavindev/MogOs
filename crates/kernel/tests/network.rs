//! Sockets over the loopback pair, through `Network`'s API as the board drives it.

use kernel::handle::{CONNECT, LISTEN};
use kernel::network::{
    Completion, Network, OP_ACCEPT, OP_CONNECT, OP_RECEIVE, OP_SEND, Owner, SOCKET_FRAMES, Sock,
    UserMemory, frames,
};
use kernel::syscall::{EACCES, EADDRINUSE, EBADF, EBUSY, ENOBUFS};
use mm::Budget;
use net::{Mac, Nic};

const ME: Owner = (1, 1);
const LOCALHOST: u64 = 0x7f00_0001;

/// User memory as one arena: a pointer is an offset into it.
struct Arena(Vec<u8>);

impl UserMemory for Arena {
    fn bytes(&self, ptr: u64, len: usize) -> Option<&[u8]> {
        self.0.get(ptr as usize..ptr as usize + len)
    }

    fn bytes_mut(&mut self, ptr: u64, len: usize) -> Option<&mut [u8]> {
        self.0.get_mut(ptr as usize..ptr as usize + len)
    }
}

/// No NIC: these tests use loopback only.
struct NoNic;

impl Nic for NoNic {
    fn mac(&self) -> Mac {
        [2, 0, 0, 0, 0, 9]
    }
    fn mtu(&self) -> usize {
        1500
    }
    fn transmit(&mut self, _: usize, _: impl FnOnce(&mut [u8])) -> bool {
        false
    }
    fn receive(&mut self, _: impl FnOnce(&[u8])) -> bool {
        false
    }
}

struct World {
    network: Network,
    budget: Budget,
    user: Arena,
    now: u64,
}

impl World {
    fn new(budget: usize) -> Self {
        let memory = vec![0; frames(false) * 4096].leak();
        World {
            network: Network::new(None, [1, 2], memory).unwrap(),
            budget: Budget::new(budget),
            user: Arena(vec![0; 4096]),
            now: 0,
        }
    }

    fn socket(&mut self, rights: u64) -> Result<Sock, i64> {
        self.network.socket(ME, rights, &mut self.budget)
    }

    fn submit(&mut self, sock: Sock, op: (u64, u64, usize, u64)) -> Result<(), i64> {
        let alive = |_| true;
        let user = &mut self.user;
        self.network.submit(sock, op, (ME, self.now), alive, user)
    }

    /// Polls until an op of `ME` finishes.
    fn complete(&mut self) -> Completion {
        for _ in 0..100 {
            self.now += 1_000_000;
            self.network.poll(None::<&mut NoNic>, self.now);
            if let Some(done) = self.network.complete(ME, &mut self.budget, &mut self.user) {
                return done;
            }
        }
        panic!("no completion");
    }

    /// A listener on `port` and a connection to it: (listener, client, accepted).
    fn connected(&mut self, port: u16) -> (Sock, Sock, Sock) {
        let listener = self.socket(LISTEN).unwrap();
        self.network.bind(listener, port).unwrap();
        self.network.listen(listener).unwrap();
        let client = self.socket(CONNECT).unwrap();
        self.submit(client, (OP_CONNECT, LOCALHOST, port.into(), 1))
            .unwrap();
        self.submit(listener, (OP_ACCEPT, 0, 0, 2)).unwrap();
        let mut accepted = None;
        for _ in 0..2 {
            let done = self.complete();
            match done.tag {
                1 => assert_eq!(done.result, 0),
                _ => accepted = done.accepted,
            }
        }
        (listener, client, accepted.unwrap())
    }
}

#[test]
fn a_loopback_connection_carries_data_both_ways() {
    let mut w = World::new(64);
    let (_, client, server) = w.connected(9);
    w.user.0[..5].copy_from_slice(b"hello");
    w.submit(client, (OP_SEND, 0, 5, 3)).unwrap();
    w.submit(server, (OP_RECEIVE, 100, 64, 4)).unwrap();
    let mut got = vec![];
    while got.len() < 2 {
        got.push(w.complete());
    }
    let received = got.iter().find(|d| d.tag == 4).unwrap();
    assert_eq!(received.result, 5);
    assert_eq!(&w.user.0[100..105], b"hello");
    assert_eq!(got.iter().find(|d| d.tag == 3).unwrap().result, 5);
}

#[test]
fn a_closed_socket_is_unreachable_and_names_its_owner_for_the_refund() {
    let mut w = World::new(SOCKET_FRAMES * 2);
    let a = w.socket(CONNECT).unwrap();
    let b = w.socket(CONNECT).unwrap();
    assert_eq!(w.socket(CONNECT), Err(ENOBUFS));
    assert_eq!(w.network.close(a), Some(ME));
    w.budget.refund(SOCKET_FRAMES);
    assert_eq!(w.network.bind(a, 1), Err(EBADF));
    // The freed entry is reused under a new generation, which the old value never reaches.
    let c = w.socket(CONNECT).unwrap();
    assert_eq!(c.index, a.index);
    assert_ne!(c.generation, a.generation);
    assert_eq!(w.network.bind(a, 1), Err(EBADF));
    assert_eq!(w.network.bind(b, 1), Ok(()));
}

#[test]
fn rights_ports_and_op_slots_are_checked() {
    let mut w = World::new(64);
    let connect_only = w.socket(CONNECT).unwrap();
    w.network.bind(connect_only, 5).unwrap();
    assert_eq!(w.network.listen(connect_only), Err(EACCES));
    let listen_only = w.socket(LISTEN).unwrap();
    assert_eq!(
        w.submit(listen_only, (OP_CONNECT, LOCALHOST, 5, 0)),
        Err(EACCES)
    );
    let (listener, client, _) = w.connected(6);
    let again = w.socket(LISTEN).unwrap();
    w.network.bind(again, 6).unwrap();
    assert_eq!(w.network.listen(again), Err(EADDRINUSE));
    w.submit(client, (OP_RECEIVE, 0, 64, 7)).unwrap();
    assert_eq!(w.submit(client, (OP_RECEIVE, 0, 64, 8)), Err(EBUSY));
    // Closing the listener frees its port.
    w.network.close(listener);
    assert_eq!(w.network.listen(again), Ok(()));
}
