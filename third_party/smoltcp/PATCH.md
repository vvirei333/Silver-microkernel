# WerewolfOS's local patch of smoltcp 0.14.0

The crate in this directory is smoltcp 0.14.0 (0BSD, see
`LICENSE-0BSD.txt`) with two small fixes. Both address an assertion in
`iface/neighbor.rs` (`lookup` requires a unicast protocol address) that the
DHCP client can trigger:

- `iface/interface/mod.rs` (`has_neighbor`): a broadcast or multicast
  destination is reported as "no neighbour needed" instead of being passed to
  the neighbour cache.
- `iface/socket_meta.rs` (`neighbor_missing`): a socket is never parked
  waiting on a broadcast/multicast neighbour.

Without the patch, a DHCP boot panics intermittently while the neighbour
cache is asked about `255.255.255.255`. The patch is two guarded early
returns; nothing else in the crate is changed.

The fix is a candidate for an upstream report. Keep this directory in sync
when moving to a newer smoltcp release.
