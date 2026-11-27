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
- A proposal qualifies when exactly two directors have voted YES and the seat
  has not voted. It qualifies immediately once every director has cast a
  ballot, whether NO or ABSTAIN. If any director ballot is missing, it
  qualifies once the response window has elapsed, whatever the other directors
  did. One YES, or three or more, never qualifies.
- Configure the contract as the proposal module's vetoer to enable veto.
  The module applies its own veto window, status transition, and completion hooks.
  A veto does not require a voting seat.

Both sudo actions take a proposal ID and the reviewed start height. They check
the bound modules at execution. Regular proposal and ballot storage remains
owned by the single-choice proposal module.
