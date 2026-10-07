//! Sockets over the loopback pair, through `Network`'s API as the board drives it.

use std::collections::HashMap;

use kernel::handle::{CONNECT, LISTEN, READ, Rights};
use kernel::network::{
    BACKLOG, Budgets, Completion, Network, OP_ACCEPT, OP_CONNECT, OP_RECEIVE, OP_SEND, Owner,
    SOCKET_FRAMES, Sock, UserMemory, frames,
};
use kernel::syscall::{EACCES, EADDRINUSE, EBADF, EBUSY, ENOBUFS};
use mm::Budget;
use net::{Mac, Nic};

const ME: Owner = (1, 1);
const OTHER: Owner = (2, 1);
const LOCALHOST: u64 = 0x7f00_0001;
const ALL: Rights = u64::MAX;

/// User memory as one arena: a pointer is an offset into it.
struct Arena(Vec<u8>);

impl UserMemory for Arena {
    fn read(&mut self, ptr: u64, dst: &mut [u8]) -> bool {
        let src = self.0.get(ptr as usize..ptr as usize + dst.len());
        src.map(|src| dst.copy_from_slice(src)).is_some()
    }

    fn writable(&mut self, ptr: u64, len: usize) -> bool {
        ptr as usize + len <= self.0.len()
    }

    fn write(&mut self, ptr: u64, src: &[u8]) -> bool {
        let dst = self.0.get_mut(ptr as usize..ptr as usize + src.len());
        dst.map(|dst| dst.copy_from_slice(src)).is_some()
    }
}

/// Every process's budget, by index.
struct Processes(HashMap<usize, Budget>);

impl Budgets for Processes {
    fn charge(&mut self, index: usize, frames: usize) -> bool {
        self.0.get_mut(&index).unwrap().charge(frames)
    }

    fn refund(&mut self, index: usize, frames: usize) {
        self.0.get_mut(&index).unwrap().refund(frames);
    }

    fn alive(&mut self, owner: Owner) -> bool {
        self.0.contains_key(&owner.0)
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
    processes: Processes,
    user: Arena,
    now: u64,
}

impl World {
    fn new(budget: usize) -> Self {
        let memory = vec![0; frames(false) * 4096].leak();
        let processes = [ME, OTHER].map(|o| (o.0, Budget::new(budget)));
        World {
            network: Network::new(None, [1, 2], memory).unwrap(),
            processes: Processes(processes.into_iter().collect()),
            user: Arena(vec![0; 4096]),
            now: 0,
        }
    }

    fn used(&self, owner: Owner) -> usize {
        let budget = &self.processes.0[&owner.0];
        budget.limit() - budget.remaining()
    }

    fn socket(&mut self, owner: Owner, rights: Rights) -> Result<Sock, i64> {
        self.network.socket(owner.0, rights, &mut self.processes)
    }

    /// Closes `owner`'s one handle to `sock`.
    fn close(&mut self, sock: Sock, owner: Owner) {
        self.network.close(sock, owner.0, true, &mut self.processes);
    }

    fn listen(&mut self, sock: Sock, backlog: usize) -> Result<(), i64> {
        self.network.listen(sock, backlog, &mut self.processes)
    }

    fn submit(&mut self, owner: Owner, sock: Sock, op: (u64, u64, usize, u64)) -> Result<(), i64> {
        self.submit_with(owner, sock, op, ALL)
    }

    fn submit_with(
        &mut self,
        owner: Owner,
        sock: Sock,
        op: (u64, u64, usize, u64),
        rights: Rights,
    ) -> Result<(), i64> {
        let (user, processes) = (&mut self.user, &mut self.processes);
        self.network
            .submit(sock, op, rights, (owner, self.now), processes, user)
    }

    fn poll(&mut self) {
        self.now += 1_000_000;
        self.network.poll(None::<&mut NoNic>, self.now);
    }

    /// Polls until an op of `owner` finishes.
    fn complete(&mut self, owner: Owner) -> Completion {
        for _ in 0..100 {
            self.poll();
            let (user, processes) = (&mut self.user, &mut self.processes);
            if let Some(done) = self.network.complete(owner, processes, user) {
                return done;
            }
        }
        panic!("no completion");
    }

    /// A listener of `ME` on `port`, and `n` connections of `OTHER` to it, established.
    fn listen_and_connect(&mut self, port: u16, n: usize) -> (Sock, Vec<Sock>) {
        let listener = self.socket(ME, LISTEN).unwrap();
        self.network.bind(listener, port, false).unwrap();
        self.listen(listener, BACKLOG).unwrap();
        let clients: Vec<_> = (0..n)
            .map(|i| {
                let client = self.socket(OTHER, CONNECT).unwrap();
                let op = (OP_CONNECT, LOCALHOST, port.into(), i as u64);
                self.submit(OTHER, client, op).unwrap();
                client
            })
            .collect();
        for _ in 0..n {
            self.complete(OTHER);
        }
        (listener, clients)
    }

    /// `ME`'s next accepted connection on `listener`.
    fn accept(&mut self, listener: Sock) -> Sock {
        self.submit(ME, listener, (OP_ACCEPT, 0, 0, 99)).unwrap();
        self.complete(ME).accepted.unwrap().0
    }
}

#[test]
fn a_loopback_connection_carries_data_both_ways() {
    let mut w = World::new(256);
    let (listener, clients) = w.listen_and_connect(9, 1);
    let server = w.accept(listener);
    w.user.0[..5].copy_from_slice(b"hello");
    w.submit(OTHER, clients[0], (OP_SEND, 0, 5, 3)).unwrap();
    assert_eq!(w.complete(OTHER).result, 5);
    w.submit(ME, server, (OP_RECEIVE, 100, 64, 4)).unwrap();
    assert_eq!(w.complete(ME).result, 5);
    assert_eq!(&w.user.0[100..105], b"hello");
}

#[test]
fn a_closed_socket_is_unreachable_and_refunds_its_owner() {
    let mut w = World::new(SOCKET_FRAMES * 2);
    let a = w.socket(ME, CONNECT).unwrap();
    let b = w.socket(ME, CONNECT).unwrap();
    assert_eq!(w.socket(ME, CONNECT), Err(ENOBUFS));
    w.close(a, ME);
    assert_eq!(w.used(ME), SOCKET_FRAMES);
    assert_eq!(w.network.bind(a, 1, false), Err(EBADF));
    // The freed entry is reused under a new generation, which the old value never reaches.
    let c = w.socket(ME, CONNECT).unwrap();
    assert_eq!(c.index, a.index);
    assert_ne!(c.generation, a.generation);
    assert_eq!(w.network.bind(a, 1, false), Err(EBADF));
    assert_eq!(w.network.bind(b, 1, false), Ok(()));
}

#[test]
fn rights_ports_and_op_slots_are_checked() {
    let mut w = World::new(256);
    let connect_only = w.socket(ME, CONNECT).unwrap();
    w.network.bind(connect_only, 5, false).unwrap();
    assert_eq!(w.listen(connect_only, 1), Err(EACCES));
    let listen_only = w.socket(ME, LISTEN).unwrap();
    let connect = (OP_CONNECT, LOCALHOST, 5, 0);
    assert_eq!(w.submit(ME, listen_only, connect), Err(EACCES));
    let (listener, clients) = w.listen_and_connect(6, 1);
    let again = w.socket(ME, LISTEN).unwrap();
    w.network.bind(again, 6, false).unwrap();
    assert_eq!(w.listen(again, 1), Err(EADDRINUSE));
    w.submit(OTHER, clients[0], (OP_RECEIVE, 0, 64, 7)).unwrap();
    let second = w.submit(OTHER, clients[0], (OP_RECEIVE, 0, 64, 8));
    assert_eq!(second, Err(EBUSY));
    // Closing the listener frees its port.
    w.close(listener, ME);
    assert_eq!(w.listen(again, 1), Ok(()));
}

#[test]
fn an_accepted_handle_gets_no_more_rights_than_the_accepting_one() {
    let mut w = World::new(256);
    let (listener, _) = w.listen_and_connect(10, 1);
    w.submit_with(ME, listener, (OP_ACCEPT, 0, 0, 1), READ)
        .unwrap();
    assert_eq!(w.complete(ME).accepted.unwrap().1, READ);
}

#[test]
fn a_closed_listener_leaves_nothing_behind() {
    let mut w = World::new(1024);
    let (listener, clients) = w.listen_and_connect(13, 2);
    // Sockets made before the close, so the old listener's entry is not reused.
    let next = w.socket(OTHER, LISTEN).unwrap();
    let client = w.socket(OTHER, CONNECT).unwrap();
    w.close(listener, ME);
    assert_eq!(w.used(ME), 0, "the listener and its backlog are refunded");
    for (i, &client) in clients.iter().enumerate() {
        w.submit(OTHER, client, (OP_RECEIVE, 0, 64, i as u64))
            .unwrap();
        assert!(w.complete(OTHER).result < 0, "a queued connection is reset");
    }
    // The port is free, and connections to a new listener (likely on the old one's TCP slot) are its owner's alone.
    w.network.bind(next, 13, false).unwrap();
    w.listen(next, 1).unwrap();
    w.submit(OTHER, client, (OP_CONNECT, LOCALHOST, 13, 7))
        .unwrap();
    assert_eq!(w.complete(OTHER).result, 0);
    w.submit(OTHER, next, (OP_ACCEPT, 0, 0, 8)).unwrap();
    assert!(w.complete(OTHER).accepted.is_some());
    assert_eq!(w.used(ME), 0, "nothing is charged to the old owner");
    assert!(
        w.network
            .complete(ME, &mut w.processes, &mut w.user)
            .is_none()
    );
}

#[test]
fn the_backlog_is_charged_at_listen_and_bounds_queued_connections() {
    let mut w = World::new(1024);
    let (listener, clients) = w.listen_and_connect(11, BACKLOG + 2);
    // The listener and its backlog, paid at `listen`; peers queued 8 and the two past it were reset.
    assert_eq!(w.used(ME), (1 + BACKLOG) * SOCKET_FRAMES);
    for (i, &client) in clients.iter().enumerate() {
        w.submit(OTHER, client, (OP_RECEIVE, 0, 64, i as u64))
            .unwrap();
    }
    for _ in 0..2 {
        let reset = w.complete(OTHER).result < 0;
        assert!(reset, "a connection past the backlog is reset");
    }
    // An accepted connection is a socket of its own, charged to the accepter.
    let accepted = w.accept(listener);
    assert_eq!(w.used(ME), (2 + BACKLOG) * SOCKET_FRAMES);
    // Closing the listener resets what it still queues and refunds it with its backlog.
    w.close(listener, ME);
    assert_eq!(w.used(ME), SOCKET_FRAMES);
    w.close(accepted, ME);
    assert_eq!(w.used(ME), 0);
}

#[test]
fn a_listen_its_holders_cannot_all_pay_for_charges_nobody() {
    let mut w = World::new(SOCKET_FRAMES * 4);
    let listener = w.socket(ME, LISTEN).unwrap();
    w.network.bind(listener, 14, false).unwrap();
    assert_eq!(w.listen(listener, BACKLOG), Err(ENOBUFS));
    assert_eq!(w.used(ME), SOCKET_FRAMES);
    // OTHER holds it too (a spawn moved it a handle, OTHER paid); a backlog of 2 fits ME but not OTHER after it.
    let cost = w.network.cost(listener);
    assert!(w.processes.charge(OTHER.0, cost));
    assert!(!w.network.hold(listener, OTHER.0));
    w.processes.charge(OTHER.0, SOCKET_FRAMES * 2);
    assert_eq!(w.listen(listener, 2), Err(ENOBUFS));
    assert_eq!(w.used(ME), SOCKET_FRAMES, "ME's backlog charge was undone");
    assert_eq!(w.listen(listener, 1), Ok(()));
    assert_eq!(w.used(ME), 2 * SOCKET_FRAMES);
    assert_eq!(w.used(OTHER), 4 * SOCKET_FRAMES);
}

#[test]
fn a_socket_is_charged_to_every_process_holding_it_until_its_last_handle_there_closes() {
    let mut w = World::new(64);
    let sock = w.socket(ME, CONNECT).unwrap();
    // A spawn moves ME's handle to OTHER: OTHER pays first, holds, then ME, holding none, is refunded.
    let cost = w.network.cost(sock);
    assert!(w.processes.charge(OTHER.0, cost));
    assert!(!w.network.hold(sock, OTHER.0));
    w.network.unhold(sock, ME.0, &mut w.processes);
    assert_eq!((w.used(ME), w.used(OTHER)), (0, SOCKET_FRAMES));
    // A refund happens once, whatever releases ME's handles.
    w.network.unhold(sock, ME.0, &mut w.processes);
    assert_eq!(w.used(ME), 0);
    // A second handle in OTHER (`dup`) costs nothing; closing one of two keeps the charge.
    w.network.open(sock);
    w.network.close(sock, OTHER.0, false, &mut w.processes);
    assert_eq!(w.used(OTHER), SOCKET_FRAMES);
    w.close(sock, OTHER);
    assert_eq!(w.used(OTHER), 0);
    assert_eq!(
        w.network.bind(sock, 1, false),
        Err(EBADF),
        "the last handle closed it"
    );
}
