# dao-dao-core

[![dao-dao-core on crates.io](https://img.shields.io/crates/v/dao-dao-core.svg?logo=rust)](https://crates.io/crates/dao-dao-core)
[![docs.rs](https://img.shields.io/docsrs/dao-dao-core?logo=docsdotrs)](https://docs.rs/dao-dao-core/latest/dao_dao_core/index.html)

This contract is the core module for all DAO DAO DAOs. It handles
management of voting power and proposal modules, executes messages,
and holds the DAO's treasury.

For more information about how these modules fit together see
[this wiki page](https://github.com/DA0-DA0/dao-contracts/wiki/DAO-DAO-Contracts-Design).

In addition to the wiki spec this contract may also pause. To do so a
`Pause` message must be executed by a proposal module. Pausing the
core module will stop all actions on the module for the duration of
the pause.

## Developing
Core messages and interfaces are defined in the [dao-interfaces](../../packages/dao-interface) package. If you are building new modules or a contract that interacts with a DAO, use `dao-interface`.

## Treasury management

For management of non-native assets this contract maintains a list of
[cw20](https://github.com/CosmWasm/cw-plus/tree/1568d9f7796ef93747e5e5e45484447fddbea80b/packages/cw20)
and
[cw721](https://github.com/CosmWasm/cw-nfts/tree/c7be7aba9fb270abefee5a3696be62f2736592a0/packages/cw721)
tokens who's balances the DAO would like to track. This allows
frontends to list these tokens in the DAO's treasury. This tracking is
needed as, for non-native tokens, there is no on-chain data source for
all of the cw20 and cw721 tokens owned by a DAO. It may also help
reduce spam as random shitcoins sent to the DAO won't be displayed in
treasury listings, unless the DAO approves them.

For native tokens we do not need this additional tracking step, as
native token balances are stored in the [bank
module](https://github.com/cosmos/cosmos-sdk/tree/main/x/bank). Thus,
for those tokens frontends can query the chain directly to discover
which tokens the DAO owns.

### Managing the treasury

There are two ways that a non-native token may be added to the DAO
treasury.

If `automatically_add_[cw20s|cw721s]` is set to true in the [DAO's
config](https://github.com/DA0-DA0/dao-contracts/blob/74bd3881fdd86829e5e8b132b9952dd64f2d0737/contracts/dao-dao/src/state.rs#L16-L21),
the DAO will add the token to the treasury upon receiving the token
via cw20's `Send` method and cw721's `SendNft` method.

```
pub enum ExecuteMsg {
    /// Executed when the contract receives a cw20 token. Depending on
    /// the contract's configuration the contract will automatically
    /// add the token to its treasury.
    #[cfg(feature = "cw20")]
    Receive(cw20::Cw20ReceiveMsg),
    /// Executed when the contract receives a cw721 token. Depending
    /// on the contract's configuration the contract will
    /// automatically add the token to its treasury.
    ReceiveNft(cw721::Cw721ReceiveMsg),
	// ...
}
```

The DAO may always add or remove non-native tokens via the
`UpdateCw20List` and `UpdateCw721List` methods:

```rust
pub enum ExecuteMsg {
    /// Updates the list of cw20 tokens this contract has registered.
    #[cfg(feature = "cw20")]
    UpdateCw20List {
        to_add: Vec<String>,
        to_remove: Vec<String>,
    },
    /// Updates the list of cw721 tokens this contract has registered.
    UpdateCw721List {
        to_add: Vec<String>,
        to_remove: Vec<String>,
    },
	// ...
}
```

## Phoenix adoption builds

This crate builds in two forms. The default build adopts the deployed
2.4.0 core. The build with the `phoenix-rotation` feature, the governance
build, does the same and adds the host-authorized governance controls: the
`sudo` membership actions, the governance seat and delayed send-as-DAO.

The two builds are alternatives. Each one's `migrate` accepts only the
unmigrated upstream 2.4.0 core, and runs once. A core adopted with the
default build cannot migrate to the governance build, so choose the
build before adoption. Adding the controls to an adopted core needs a
new build that is reviewed separately.

### Both builds

Adoption removes the outgoing member and sets a proposal snapshot boundary.
A proposal that is unresolved at a boundary, or created in the same block
after it, can no longer dispatch its messages. Any number of proposals can
be blocked this way. Adoption takes the reviewed lists of group hooks and
of the proposal module's proposal hooks and vote hooks, and fails if a
live list differs.

Opening proposals cannot stop adoption. Every proposal up to the reviewed
count must be resolved, and that reviewed range must fit in 512 proposals.
Adoption classifies at most 512 proposals in all and reports any it leaves
as `unscanned_proposals`. Those are classified afterwards, before anything
can execute.

The pinned proposal module's execution hook carries a proposal's messages
and no ID, so blocked proposals are filed under their messages. A later
proposal with the same messages executes normally. An execution reads at
most 128 entries filed under its own messages and drops the settled ones.
It fails while more than 64 of them can still execute, or while more than
128 are filed. Each successful execution also retires up to four settled
entries, whatever its messages, so such a payload becomes usable again as
its blocked proposals settle.

### Default build

Adoption sets the only boundary. A later roster change made by an ordinary
proposal sets none, so proposals created under the previous roster can
still execute. There is no `sudo` entry point and no checkpoint action.
If adoption leaves proposals unclassified, or a long run of proposals is
created in the adoption block after the adoption transaction, the first
execution afterwards files all of them and needs a correspondingly high
gas limit.

### Governance build

An ordinary proposal can change director membership within the roster
limits. That establishes a new proposal snapshot boundary, as do the
governance membership actions and a roster change made through
send-as-DAO. Governance controls when the voting proxy joins or leaves. A
seated voting proxy requires revoting to be disabled and protects the
voting period from ordinary proposal changes. For a height-based voting
period, the period must extend past the proxy's response window.

This build reserves administration of its pinned voting, group, and
proposal contracts for governance. Ordinary proposals can still dispatch
messages to those contracts through their existing execute interfaces. The
same check applies to the callback messages of a proposal module that an
ordinary proposal adds. The dispatch check reserves membership group hooks
and its internal admin for governance, and likewise the proposal module's
proposal hooks, vote hooks and pre-propose module. A hook runs on every
status change or vote, so one could make a veto or the voting proxy's
deciding vote fail.

When migration installs a governance veto, it requires a nonzero timelock
and veto-before-passed support. The configured vetoer can therefore use the
proposal module's ordinary veto while a proposal is open or during its
post-passage timelock. While a veto is configured, ordinary proposals
cannot change the maximum or minimum voting period, so they cannot shorten
the time a proposal stays open to a veto. The timelock itself is whatever
the reviewed adoption message sets; no minimum is enforced, so review it
against the time governance needs to act.

The voting proxy's response window is counted in blocks. With a
height-based voting period the window must end before the period does.
With a time-based voting period nothing checks this, so choose a window
safely shorter than the voting period at the chain's slowest expected
block time. Otherwise a proposal can expire before the proxy may decide a
partial ballot.

These controls bind only once governance is the core's host admin. With
`KeepCurrentAdmin` the core stays its own admin and the DAO keeps upgrade
authority, so an ordinary proposal can migrate the core or reassign its
admin and remove every control. Use `GovernanceOnly`, or have governance
call `SetMigrationAdmin` after adoption.

A proposal's last message, and a send-as-DAO action's last message, are
followed by a reply to the core (IDs 4856 and 4857). If the batch migrates
the core, that reply is delivered to the successor code, and a successor
that does not handle it fails the whole batch. To move a self-administered
core to such code, reassign its host admin in one proposal and migrate
from that account.

A membership change scans at most 512 proposals created since the previous
one. Past that, every membership change fails, including one made by an
ordinary proposal, until governance has run `CheckpointProposalHistory`
over the backlog. Proposals opened while a governance vote is running can
outpace the checkpoints bundled with a membership action. The action then
fails and has to be retried with more checkpoints. The same checkpoints
classify what adoption left unscanned; until they have, executions fail.
Each call classifies up to 48 proposals, advances a long
same-block tail, and prunes up to 48 settled entries. Its event reports
`checked_through` against `proposal_count`, the `boundary_cursor`, the
number `pruned`, and `prune_complete`, which is `false` while the pruning
walk has not reached the end of the index. A proposal that a checkpoint
has staged still executes under its current roster until the next
boundary. An execution fails while more than 64 staged proposals carry its
messages.

A boundary event lists the proposals it blocks in `gated_proposals`: the
newly scanned ones, and those among the first 12 staged ones that can
still execute. `gated_proposals_truncated` is set when more staged
proposals were blocked than the list shows.
