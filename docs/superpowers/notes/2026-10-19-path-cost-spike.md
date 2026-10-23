# Path-cost spike: keepalive cost and NAT hold times

Measured 2026-10-20 with `crates/oxutrm-net/examples/path-cost.rs` (branch
`design/live-paths`), as plan Task 6 of
`docs/superpowers/plans/2026-10-19-live-paths-stage1.md` asks. Input for the
background-traffic budget of the live-paths spec, §5.4.

## Cost of a kept path (`idle --secs 60`, loopback)

Shipped transport config (10 s keepalive). Pings are 16-byte QUIC datagrams,
echoed. Per direction (both directions measured the same to within 0.1 B/s).

| ping rate | datagrams/s | UDP payload B/s | on wire IPv4 B/s | on wire IPv6 B/s |
|---|---|---|---|---|
| 0 Hz (keepalive alone) | 0.23 | 7.5 | 14.0 | 18.6 |
| 0.2 Hz | 0.43 | 16.7 | 28.8 | 37.4 |
| 0.5 Hz | 1.04 | 41.0 | 70.1 | 91.0 |
| 1 Hz | 2.02 | 80.6 | 137.0 | 177.4 |
| 2 Hz | 4.02 | 161.5 | 273.9 | 354.3 |

Every ping costs about two datagrams each way (the ping or its echo, plus the
ACK), so the cost is linear in the ping rate on top of the keepalive.

## NAT hold times (`hold`, Mac ↔ thinlinc)

`hold` measures the NAT in front of the **listener**. "Longest arrived" and
"shortest lost" bound the idle lifetime of a mapping. Three rounds with
different hold sets (15–480 s, 20–90 s, 20–300 s); the table merges them.

| link class | listener | longest arrived | shortest lost | lifetime |
|---|---|---|---|---|
| public IPv4 (both NATs) | Mac | 120 s | 300 s | 120–300 s |
| public IPv4 (both NATs) | thinlinc | 20 s | 30 s | **20–30 s** |
| IPv6 (thinlinc NAT66) | Mac | 60 s | 480 s | 60–480 s |
| IPv6 (thinlinc NAT66) | thinlinc | 20 s | 50 s | **20–50 s** |
| VPN IPv4 (WireGuard) | Mac | 20 s (30 s once) | 30 s (60 s once) | **20–60 s** |
| VPN IPv4 (WireGuard) | thinlinc | 480 s | — | > 480 s |

Observations:

- The shortest lifetime is on thinlinc's side of the public paths: its site
  router forgets an idle UDP mapping after 20–30 s in IPv4 and after 20–50 s
  through NAT66. The VPN path also expires on the Mac's side after 20–60 s, the
  30 s hold arriving in one round and getting lost in another, so the edge sits
  near 30 s.
- `lost` is ambiguous (the sender's own NAT may have re-mapped as well), but
  every short lifetime above is bracketed by an `arrived` at 20 s on the same
  socket set, and in the reverse direction the same NAT, now on the sending
  side, did not stop longer holds from arriving.
- Over NAT66 the multi-port punch was unreliable: across two rounds, 8 of 24
  sockets never punched (both ends sent, nothing came through), while public
  IPv4 and VPN IPv4 punched 24 of 24. Whatever causes it, the NAT66 path is less
  predictable per port than the IPv4 NAT, which the live-paths prober should
  expect.

## Recommendation for the §5.4 budget

- **Keepalive interval: keep 10 s.** Shortest lifetime ÷ 2 = 20 s ÷ 2 = 10 s,
  which is today's figure exactly. Do not lengthen it: a 15 s keepalive would
  already fall inside the 20–30 s window seen on the public IPv4 path. Both ends
  send keepalives and every PING is ACKed, so each NAT sees traffic in both
  directions at least every 10 s.
- **Measurement ping rate: 0.2 Hz** (one ping per 5 s) for a kept,
  non-active path. That is enough for an rtt/loss estimate on the comparison
  rule's time scale and doubles the keepalive cost; 1 Hz would cost ten times
  the keepalive for no decision the comparison rule needs faster.
- **Resulting cost per direction, on wire, IPv6 worst case:**

  | kept paths | keepalive only | + 0.2 Hz ping |
  |---|---|---|
  | 1 | 19 B/s | 37 B/s |
  | 2 | 37 B/s | 75 B/s |
  | 4 | 74 B/s | 150 B/s |
  | 8 | 149 B/s | 299 B/s |

  Even 8 kept paths at 0.2 Hz cost about 0.3 kB/s each way, about 3.4
  datagrams/s. The byte cost is negligible on any link oxutrm targets. The
  argument for a cap is the datagram rate on metered or battery-bound
  links and the noise in NAT tables, not bandwidth.
