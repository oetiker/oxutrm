# The PTO backoff bound, and why oxutrm needs a patched quinn

**Status: oxutrm currently builds against a patched `quinn-proto`.** The patch
adds one `TransportConfig` knob. It is not in released quinn, and this file is
the record of why it exists, what it is worth, and what has to happen for it to
go away.

## The bug it fixes

A session whose network path disappears for a few minutes and then **comes
back** does not resume. Both ends are alive, the host is still registered and
merely detached, the path is perfect again — and the terminal stays dead, for
minutes.

Found by hand against a real host over a VPN on 2026-08-30
(`docs/superpowers/notes/2026-08-30-tier-a-hand-test.md`), where a 7-minute
outage had still not recovered two minutes after the VPN came back, and the
notice's `sent` counter — which is quinn's own `stats.path.sent_packets` — was
advancing by about **one packet every two minutes**.

## Root cause

`max_idle_timeout(None)` removed the death sentence it was removed for. It also
removed, unnoticed, the only thing bounding quinn's **exponential PTO backoff**.

The probe interval is `pto_base * 2^min(pto_count, MAX_BACKOFF_EXPONENT)`, and
quinn's `MAX_BACKOFF_EXPONENT` is **16** — a cap of 65536x the base. On an
ordinary path with a 20 ms base that is a probe timer of over twenty minutes.
Normally this never matters, because the idle timeout closes such a connection
long before the backoff gets that far. With no idle timeout, nothing does.

This is the project's existing lesson — *removing a timeout unbounds every
await that depended on it* — one layer below where it was first learned.

## What was measured, and what was ruled out

Against `session::tests::blackout_recovery_curve`, a UDP relay that really
drops packets. `SIGSTOP` cannot produce this: a stopped process still has a
kernel receiving into its socket buffer, so on resume it drains and ACKs the
backlog and recovery is instant. A 60 s `SIGSTOP` "outage" recovers in 0 s.

| blackout | exponent 16 (default) | exponent 6 |
|---|---|---|
| 15 s | 2.13 s | **0.55 s** |
| 60 s | 10.19 s | **0.55 s** |
| 150 s | 106.65 s | **1.24 s** |

Two cheaper fixes were tried first and **both are refuted**, which is why this
patch exists rather than one of them:

- **Rebind while `Silent`, not only when the route IP moved.** Reuses machinery
  that already exists in `follow_route`. It does nothing: 14.65 s against
  10.19 s at 60 s, 98.87 s against 106.65 s at 150 s. quinn resets loss state
  when the **peer's** address changes, not when it rebinds its own socket, so
  the migration reset this hoped for never happens.
- **`Connection::ping()`**, exposed by vendoring quinn (`quinn_proto` has it;
  `quinn` 0.11.11 does not re-export it). Also does nothing: 10.35 s against
  10.19 s at 60 s, 116.01 s against 106.65 s at 150 s. `ping()` sets
  `ping_pending`, which makes the *next* transmit ACK-eliciting — it does not
  create a transmit opportunity, and the missing transmit opportunity is the
  entire problem.

## The patch

One field on `quinn_proto::TransportConfig`, defaulting to 16 so nothing
changes for anyone who does not set it:

```rust
pub fn max_backoff_exponent(&mut self, value: u32) -> &mut Self
```

and `pto_time_and_space` reads `self.config.max_backoff_exponent` where it read
the `MAX_BACKOFF_EXPONENT` constant.

oxutrm sets **6**: the probe interval tops out at 64x the base, a few seconds on
a real path, so a returning path is noticed in about that long however long the
outage was. It costs probe packets during an outage — 441 across a 150 s
blackout against 313 at the default, about 3 a second — and **nothing at all on
a healthy connection**, where `pto_count` is zero.

`crates/oxutrm-net/src/quic.rs`'s
`the_probe_backoff_is_bounded_now_that_nothing_else_bounds_it` asserts the
wiring off the built config's `Debug`, and fails against the default — verified
by injecting it.

## What has to happen next

1. **Upstream it.** The argument that generalises: any application that
   disables the idle timeout in order to survive long outages inherits an
   unbounded probe timer, and has no way to bound it. A defaulted knob costs
   upstream nothing.
2. **Until it lands, the `[patch.crates-io]` is load-bearing** and MUST be part
   of the build. A plain `cargo build` without it silently reverts to exponent
   16 and the bug comes back — the guard test above is what catches that.
3. **This is not a substitute for `REBUILD_AFTER`.** It fixes the case where the
   path comes back and the connection is still usable. A connection that is
   genuinely gone — the peer's address changed, the NAT binding lapsed — still
   needs Tier B's rebuild. What this removes is the far more common and far
   more infuriating case: nothing was broken except a timer.
