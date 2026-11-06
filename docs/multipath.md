# Multipath links: peers by identity, paths found and ranked

Status: proposal (2026-11-05). Pending owner review before implementation.

## Problem

A peer is configured as `host:port`. Everything about the transport follows from that one string, and
each of these failures was hit on real machines:

- **Addresses drift.** DHCP moves a machine; one peer's config still pointed at `127.0.0.1` and dialled
  a refused port every few seconds for weeks.
- **The configured path is not the good path.** A Thunderbolt cable was plugged between two Macs.
  The link stayed on Wi‑Fi, because Wi‑Fi was the address in the config. Measured over 600 pings each:
  - Wi‑Fi path: median 6 ms, 13% above 40 ms, max 168 ms.
  - Thunderbolt Bridge: median 0.6 ms, max 1 ms.
- **Wi‑Fi on these Macs stalls periodically.** AWDL takes the radio to its listening channels (6, 44,
  149) and back. Measured mini→router: with AWDL up, 10% of pings above 40 ms; with AWDL held down,
  0%. Universal Control avoids this with an AWDL "realtime mode" that has no public API (Apple DTS).
  So the transport has to prefer a wire when one exists, by itself.
- **A changed setting restarts the worker.** Every link drops and comes back.

The root is the same in all four: a peer is identified by where it was last seen, not by who it is.
The certificate fingerprint already proves who it is (pinned, mutual TLS). This design makes the
fingerprint the only thing a peer needs to be, and finds the way to it at run time.

## Design

### 1. A peer is its fingerprint

`Peer.addr` becomes an optional hint (`addr = ""` is valid). Pairing still records the address it
used. Nothing else depends on it.

### 2. Every node listens on every interface

The listener binds `0.0.0.0:<port>` on every platform. That is already true on the mbp (and is
`Config::default`); the mini bound `127.0.0.1` only; Windows was not checked. TLS pins both
certificates, so an open listener admits no one new. Every node also dials, so either side can bring a
path up.

This is a change of posture for the mini: it starts accepting connections from the LAN, where it
accepted none. Pinned mutual TLS is what makes that safe; the change is still stated to the owner,
not just made.

### 3. Where a peer can be reached comes from three sources

- **Beacon.** Each node sends a small UDP datagram every second, and at once when its interfaces change.
  - Sent as a broadcast on every up, non‑loopback IPv4 interface, to a fixed port (24790).
  - Payload: magic, protocol version, the node's 16‑byte id, its TCP port. No addresses: the receiver
    takes the datagram's **source address** as the address to dial, which is by construction an
    address that reached it.
  - Beacons from unpaired ids are ignored.
  - A beacon is only a hint. A forged one makes us dial an address, and TLS refuses it there.
- **Paths message.** Right after the Hello exchange, each end sends its addresses with each one's
  interface kind (§8). So once any path is up, both ends learn the others, even where broadcast is
  filtered.
- **Cache.** The last address that completed a handshake, per peer and per kind, is kept in
  `<state dir>/paths.toml`. It is used at start, before any beacon arrives. It is not the user's
  config file.

Each source's addresses live as long as the source vouches for them:

| Source | Valid while |
|---|---|
| Beacon | 3 s after the last datagram from that address |
| Paths message | while the session that delivered it is up; afterwards kept as a cache entry, so pulling a cable still leaves the Wi‑Fi address to fall back to |
| Cache, config hint | always a candidate, behind its own per‑address backoff (200 ms doubling to 30 s) |

At most 8 candidates per peer; a new one past the cap replaces the stalest beacon entry, so a flood of
forged beacons cannot grow the set or push out a working address.

### 4. Paths are ranked, and both ends rank them the same way

Kind of a local interface:

| Kind | macOS | Windows | Rank |
|---|---|---|---|
| Wired | Ethernet; Thunderbolt Bridge (`SCNetworkInterface` type Ethernet or Bridge) | `IF_TYPE_ETHERNET_CSMACD` | 0 |
| Other | anything not classified | — | 1 |
| Wi‑Fi | IEEE80211 | `IF_TYPE_IEEE80211` | 2 |

- The rank of a path is the **worse** of the two ends' kinds. A Mac wired to the router talking to a
  laptop on Wi‑Fi is a Wi‑Fi path. Each end reports its own kind for the connection in its Paths
  message, so both ends compute the same rank. That is what stops two ends preferring different paths
  and flapping.
- The order is `(rank, dialled by the lower node id)`, nothing else. Both are known the moment a
  handshake completes and both ends compute them identically. Measured RTT is left out on purpose: a
  new session has none yet, and comparing a number against "unknown" is how flapping starts.
- The path manager compares the session **its own dial would produce** with the current one. So the
  higher‑id node does not redial a path the lower‑id node already holds: its dial would lose the tie.

### 5. One session per peer: the best one

The route table holds at most one session per peer. Today the first session to arrive wins and a
second one is closed. Instead, when a new session comes up:

- It **replaces** the current one if it ranks strictly better (make‑before‑break). The forwarder
  never sees a gap: `link_up` is not repeated, and messages already in flight on the old session are
  delivered or dropped there.
- Otherwise it is closed, as today.

**Every per‑session teardown is keyed by `(peer, generation)`, on both ends.** When the old session
ends, the route table already holds the newer generation, and that end must do nothing that belongs
to the peer as a whole:

- no `link_down` to the forwarder (it would release a live RemoteIn on the receiver, or bring control
  home on the controller);
- no drag‑hub unregister, no clipboard‑poller stop for the new session.

Today `link_down`, the drag hub and the clipboard poller are keyed by peer name only, so this is a
change in each of them, not just in the route table.

**Upgrades wait while a file drag is transferring** to or from that peer. The drag hub's sender is
bound to the session it was attached on, and chunks in flight on the old session would be lost. An
upgrade found mid‑transfer is retried when the transfer ends.

Who dials:

- A path manager per peer looks at its candidates every 500 ms.
- It dials a candidate that ranks better than the current session, was seen in the last 3 s, and is
  out of its per‑address backoff.
- With no session at all, it dials the best candidate at once, with no backoff on the first try.

### 6. Losing a path

- The path manager watches local interfaces (500 ms poll of the interface list). If the active
  session's local address disappears, for example because the cable was pulled, it closes the session
  at once instead of waiting `dead_after` (3 s).
- A session lost mid‑control is a normal link down, as today: the forwarder sees `link_down`, the
  controller takes its input back, the receiver releases every injected key and button. Hiding the gap
  would mean keys pressed during it land on a session that cannot deliver their release.
- An **upgrade** (§5) never drops, so plugging a cable in is seamless. Pulling one out returns control
  to the local machine for about a second. That is the honest trade.

### 7. Settings apply without a restart

The worker reloads `config.toml` when it changes (mtime, polled every second). A file that does not
parse or validate, for example one caught mid‑write, is logged and ignored as a whole; the next
change is tried again. A valid one is applied as a difference, in place:

- **Peers added or removed:** their path managers start or stop. A removed peer that is being
  controlled is released first.
- **Edge, key map, switch policy:** swapped into the forwarder under its lock.
- **Listen port:** rebinds the listener. The existing sessions continue.

The settings window then only writes the file; it no longer stops and starts the worker.

### 8. Protocol

The wire encoding is postcard: positional fields, enum variants by index. A v3 node cannot decode a
Hello with a field added, or a message variant it does not know; either is a protocol fault, not a
version it can refuse politely. Hence:

- **Hello keeps its exact shape.** Only `ver` changes: 4.
- **A new message, `Paths { addrs: Vec<(SocketAddr, Kind)>, kind: Kind }`**, appended as the last `Msg`
  variant. It is sent right after the Hello exchange, and only when both ends said `ver >= 4`.
  `addrs` is capped at 16.
- **Versions are negotiated, not required equal.** A v4 node accepts a Hello of 3 or 4 and speaks the
  lower one. A v3 node refuses a v4 Hello: it requires equality, already deployed on Windows. The v4
  node records that peer's version from the refusal and redials speaking 3.
- So a v4 Mac and the v3 Windows build keep working, without multipath, until Windows is rebuilt.
- A peer older than 3 is refused, and the settings window shows the reason: 「另一台需要更新」.

### 9. Where the code goes

- **`flowseam-net::paths`** (new): candidates, ranking, the path manager loop, beacon send and
  receive, the path cache. Shared by Mac and Windows.
- **`flowseam-net::session`**: Hello v4.
- **`flowseam-mac::links`** and **`flowseam-win::app`**: replace their own dial and serve loops with the
  path manager. The route table's "first wins" becomes "best wins".
- **Interface kinds:** `flowseam-mac` (SystemConfiguration) and `flowseam-win` (GetAdaptersAddresses).

## Slices

Each slice ends demoable on the real machines.

1. **Version negotiation, Paths message, ranking and best‑wins**, Mac only. The Windows link stays
   up on v3 throughout.
   - Gate before any real‑machine test: path‑manager unit tests with a fake clock and a fake dialer.
     They cover ranking, best‑wins replacement, and no `link_down` on a superseded session at either
     end.
   - Demo: the mini and the mbp start on Wi‑Fi. With the cable plugged in, they move to Thunderbolt
     with no config edit (learned from the Paths message).
   - Pulling the cable falls back to Wi‑Fi within about 1 s.
2. **Beacons and the path cache.**
   - Demo: delete both `addr` hints. The peers still find each other, after a restart, and after the
     Thunderbolt address changes.
3. **Hot reload.**
   - Demo: change an edge in the settings window. The other links stay up (measured: session
     generation unchanged).
4. **Windows parity.** Path manager, interface kinds, a firewall rule for UDP 24790, rebuild and
   install.
   - Demo: the Windows link uses Ethernet if the PC has one, and survives a DHCP change.

## Not in this design

- **Wi‑Fi stutter without a wire.** An untethered laptop on Wi‑Fi still stalls when AWDL changes
  channel, and no public API stops it. This design makes a wire win whenever one exists, nothing more.
- **Smoothing motion over Wi‑Fi** (an adaptive playout buffer on the receiver). It trades a fixed delay
  for evenness, which is a separate product decision. It only matters when no wire exists.
- **A warm standby session** on the second‑best path, which would make pulling a cable gapless.
  - ponytail: one session per peer. Add a standby if failover mid‑control turns out to matter in use.

## Open points

- **Input in flight on a replaced session (known defect).** When a better path replaces a session, the old
  session's receiver still hands what it had queued to the forwarder, with no generation check. A key or
  button sent just before the switch can then arrive after one sent just after it on the new path, so a key
  can stay pressed until the pointer leaves this machine (leaving releases every injected key and button).
  It needs typing or a held button at the moment the path changes. Fix: hold the new session's input until
  the old one has drained, then close the old one.
- Whether broadcast reaches across the Deco mesh between Wi‑Fi clients is untested. If it does not,
  Hello and the cache still cover Wi‑Fi. Only a first contact with no hint and no cache would need it.
- Classifying Thunderbolt Bridge as "Bridge" is taken from SystemConfiguration's interface types. It
  is to be confirmed on both Macs in slice 1.
