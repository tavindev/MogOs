# `crates/net` - the network stack (L2/L3 today)

## What this crate is

`Stack<'a>` over a `Nic` port trait: Ethernet, ARP, IPv4, ICMP echo and UDP (phase 8 step 46). `Stack::new(config,
neighbors, sockets)` takes its ARP cache (`[Neighbor::EMPTY; N]`) and socket table (`Socket::new(buf)` per slot) from
the caller; then `bind` (a UDP port or an ICMP echo identifier), `close`, `send_to`, `recv_from`, and
`poll(nic, now)`, which handles every received frame and due ARP retry and returns the next deadline. `Counters`
counts frames by outcome. All in `src/lib.rs`, design notes at its top.

The `Nic` trait: `mac`, `mtu`, `transmit(len, |buf| ..)` (the stack writes the frame into the driver's buffer) and
`receive(|frame| ..)`.

It is **NOT** TCP (step 47), a driver (virtio-net is the board's, step 49), socket handles, rights or budgets (the
kernel, step 50), DHCP, DNS or IPv6 (phase 9), or IPv4 fragment reassembly.

## Responsibilities

- Parse and answer: ARP requests aimed at us, ICMP echo requests; deliver UDP datagrams and ICMP echo replies to
  bound sockets (an ICMP socket sends and receives whole echo messages, like a Linux ping socket; the stack sets the
  identifier and checksum).
- Route: on-link unicast directly, anything else off the subnet through `Config::gateway` (`NoRoute` without one).
- Resolve: `send_to` to an unknown next hop sends an ARP request and returns `Unresolved` (the datagram is not queued;
  the caller retries after `poll`). `poll` re-sends every second, gives up after 3 requests and frees the entry. A
  neighbour learned 60 s ago is asked again while still used.

## Boundaries (hard)

- `#![cfg_attr(not(test), no_std)]`, no dependencies, no `alloc`, workspace `forbid(unsafe_code)`. smoltcp is approved
  only as a host-only dev-dependency for step 47's interop tests; never a normal dependency.
- No clock: every entry point that can time out takes `now` (ns), so a seed replays a run exactly.
- Memory is the caller's: the ARP table, the socket table and each socket's receive buffer; `Stack` itself is about
  1.6 KiB (its reply buffer, `MAX_FRAME`).
- Leaf crate: the kernel (step 50) and the board (step 49) will depend on it; it depends on none of them.

## Invariants & rules

- Every frame is untrusted. Each field is range-checked once when decoded (slices via `get`/`split_at_checked`); a
  frame that fails is dropped and counted in exactly one `Counters` field (`malformed`, `checksum`, `fragments`,
  `ignored`, `no_socket`, `socket_full`), never a panic, overflow or out-of-range index.
- IPv4 fragments (MF set or an offset) are dropped and counted: no reassembly memory to exhaust.
- ARP learns only from a reply to an entry we asked about (first reply wins) and from a request aimed at our IP;
  the ARP sender MAC must equal the Ethernet source and the sender IP must be an on-link unicast address other than
  ours. Everything else is `ignored`, so ARP traffic not aimed at us never evicts a neighbour. A full cache evicts
  its least recently used entry. ARP has no checksum; these checks are what keep a flipped bit from poisoning the
  cache.
- A full socket table is `TableFull`, a bound port `InUse`; nothing is evicted silently. A full socket buffer drops
  the datagram (`socket_full`).
- Replies built while the received frame is borrowed (ARP reply, echo reply) go through one buffer, sent after the
  NIC releases the frame.
- Checksums: one pass over the payload, summed as big-endian 32-bit words. A received datagram is copied into the
  socket buffer, then committed only if its checksum holds. Copy then sum measured faster on the host than a fused
  copy-and-sum loop (64-byte receive path 31 against 35 ns), so `copy_sum` is a `memcpy` and a sum.
- Performance is the moat: a slowdown is never accepted because it has an explanation; it is removed, or shown to
  be unavoidable with before/after numbers (`docs/BENCHMARKS.md`).

## How it's tested

- Host: `cargo test --target aarch64-apple-darwin -p net`. `tests/stack.rs` runs two stacks over the simulated link
  (`tests/sim/mod.rs`: seeded loss, reordering, duplication, single-bit corruption and delay in virtual time): ping
  and UDP echo of 32 messages each over a clean link and under 10% loss, 10% reordering, 5% duplication and 5%
  corruption for 200 seeds, every reply checked byte for byte. A seeded mutation test feeds 100k mutated copies of
  the frames recorded from a clean run (bit flips, byte and 16-bit field overwrites, truncation, extension) and checks
  each is counted exactly once, and that a single checksum-detectable mutation never delivers changed data. ARP:
  spoofed and unsolicited traffic, a 10k-frame flood not aimed at us, LRU eviction, retries and the `poll` deadline.
  Also routing, named socket errors, a full socket buffer, fragments. `src/lib.rs` unit-tests the socket ring's
  wrap-around checksum.
- Benchmark: `cargo bench-host` runs `benches/net.rs` (includes `tests/sim/mod.rs`); baseline rows in
  `docs/BENCHMARKS.md`.

---

> After changing anything in this crate, run the `reviewer` pass (`docs/WORKFLOW.md`, step 5). Facts a change makes
> stale (names, signatures, constants, test names) are updated in the same commit; changing a boundary, rule or
> invariant needs explicit user approval.
