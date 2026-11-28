# Governance seat

The seat relays a host-authorized deciding vote or veto to one single-choice
proposal module. It is a distinct ballot identity with a `sudo` entry point
and no ordinary transaction entry point.

- Instantiate the seat against the DAO's CW4 group and proposal module.
  Set the equal voting weight and a positive response window in blocks.
- Seat the contract in the group for tie-breaking. The proposal snapshot must
  include the seat and three or four equal-weight directors. The proposal must
  be open under a three-YES absolute threshold, with neither delegation nor
  revoting enabled.
- Exactly two directors must have voted YES. The proposal qualifies
  immediately once every director has cast a ballot, and otherwise after the
  response window, for a board of three or four directors alike. A NO, an
  abstention and a missing ballot are treated the same: none of them can
  withhold the deciding vote. The seat must still have its ballot available.
- The response window is counted in blocks. Under a time-based voting
  period nothing checks that the window ends before the proposal expires,
  so choose a window that is safely shorter than the voting period at the
  chain's slowest expected block time.
- Configure the contract as the proposal module's vetoer to enable veto.
  The module applies its own veto window, status transition, and completion hooks.
  A veto does not require a voting seat.

Both sudo actions take a proposal ID and the reviewed start height. They check
the bound modules at execution. Regular proposal and ballot storage remains
owned by the single-choice proposal module.
