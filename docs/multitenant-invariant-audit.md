# Multi-writer invariant audit — `Event` enum

**Phase 0 deliverable** for the multi-tenant "group server" work (see
`accountir/MULTITENANT-SPEC.md` §7 Phase 0, and `ROADMAP.md` investigation #1).
Branch: `accountir-app@feature-multitenant`. Date: 2026-07-11.

## Why this exists

Today the ledger is **single-writer by construction**: one process, one SQLite
file, and `EventStore::append(&mut self, …)` — so the Rust borrow checker *and*
one-file-one-process both guarantee that no two appends interleave. Going
server-authoritative with concurrent online clients (SPEC §4.1) removes that
guarantee. This document walks every `Event` variant and records, for each, the
invariant it depends on, **where that invariant is enforced today**, and whether
that enforcement survives concurrent writers.

## Structural findings (the load-bearing ones)

1. **There is no `expected_head_seq` / compare-and-append and no per-append
   transaction.** `append()` (`src/store/event_store.rs:86`) does a single
   `INSERT`; ordering is SQLite autoincrement `id`; `latest_id()` = `MAX(id)`.
   Nothing lets a caller say "append iff head is still N." **This primitive must
   be built before any of the fixes below can be made atomic.**

2. **Every cross-event invariant is enforced by read-projection-then-append in a
   command handler**, not inside the append. Under one writer that read→append
   pair is effectively atomic; under two writers **every one of them is a
   TOCTOU race**. Examples verified by hand: the period-close fence
   (`src/commands/entry_commands.rs:160-174` reads `fiscal_periods.status` then
   decides) and the payment-ceiling guards.

3. **The hash does not chain.** `compute_event_hash(event, timestamp, user_id)`
   has no previous-hash input; the Merkle tree (`src/store/merkle.rs`) is
   rebuilt out-of-band from hashes in `id` order. It gives positional
   tamper-evidence, **not** ordering/serialization or an optimistic-concurrency
   check. The `UNIQUE(hash)` constraint only dedups a byte-identical
   (event+ts+user) resubmit — it does **not** stop two *different* writers from
   appending semantically-conflicting events.

4. **Several barrier events have no writer path yet.** `PeriodClosed`,
   `PeriodReopened`, `YearEndClosed`, `FiscalYearOpened` are defined and have
   domain logic in `src/domain/fiscal_period.rs`, but **no command emits them**.
   The reader half (the fence in `entry_commands.rs`) exists; the writer half
   doesn't. Whoever wires period-closing on the server must build the fence
   transactionally from day one.

## Per-variant table

Class taxonomy follows `ROADMAP.md`. `SW-dep` = does correctness depend on the
read-then-append being atomic (would two concurrent writers break it)?

| variant | class | invariant | enforced today (file:line) | SW-dep | server fix (inside append txn) |
|---|---|---|---|---|---|
| CompanyCreated | configuration | singleton company | `account_commands.rs:430` read-count-then-decide | MED | singleton guard in append txn |
| CompanySettingsUpdated | configuration | field names a real column | none (no command path) | NONE | last-writer-wins |
| UserAdded | configuration | username unique | `users.username UNIQUE` (`migrations.rs:194`) | NONE | keep unique index |
| UserModified | configuration | user exists | none (no command path) | NONE | last-writer-wins |
| UserRemoved | idempotent-toggle | — | none (no command path) | NONE | idempotent |
| AccountCreated | configuration/seq-alloc | account_number unique; auto `MAX+1` | `account_commands.rs:124` dup-check; alloc `:43` | **HIGH** | UNIQUE index + allocate number in txn |
| AccountUpdated | configuration | renamed number stays unique | `account_commands.rs:197` dup-check | MED | UNIQUE index on account_number |
| AccountDeactivated | stateful-exclusive | active AND zero net balance | `account_commands.rs:331` reads SUM(lines) | **HIGH** | re-check balance+active under head_seq |
| AccountReactivated | idempotent-toggle | currently inactive | `account_commands.rs:385` | NONE | idempotent |
| JournalEntryPosted | barrier-fence | debits=credits (local); accounts active; **date not in closed period**; ingest ref dedup | balance local `validation.rs:419`; active `entry_commands.rs:144`; **fence `entry_commands.rs:160`**; ref dedup `ingest_commands.rs:318` | **HIGH** | re-check fence+active under head_seq; UNIQUE index on `reference` |
| JournalEntryVoided | idempotent-toggle | exists, not already void | `entry_commands.rs:213` | MED | idempotent / re-check |
| JournalEntryUnvoided | idempotent-toggle | exists, is void | `entry_commands.rs:250` | MED | idempotent / re-check |
| JournalEntryAnnotated | append-commutative | entry exists | `entry_commands.rs:287` | NONE | commutative |
| JournalLineReassigned | stateful-exclusive | not void; line exists; new account active | `entry_commands.rs:321` multi-read | MED | re-check under head_seq |
| FiscalYearOpened | configuration | one open per year | PK `fiscal_years.year` (`migrations.rs:210`); no emitter | NONE | PK handles it |
| PeriodClosed | barrier-fence | not already closed; **establishes the fence** | domain `fiscal_period.rs:66`, **no emitter** | **HIGH** | close under head_seq so nothing lands after fence |
| PeriodReopened | barrier-fence | currently closed | domain `fiscal_period.rs:76`, **no emitter** | **HIGH** | fence under head_seq |
| YearEndClosed | stateful-exclusive | ALL periods closed first | domain `fiscal_period.rs:185`, **no emitter** | **HIGH** | re-check all-closed under head_seq |
| CurrencyEnabled | configuration | code unique | PK, `import.rs:195` | NONE | idempotent config |
| ExchangeRateRecorded | append-commutative | rate>0 (local); no uniqueness | `validation.rs:190`; no emitter in app | NONE | commutative |
| PlaidItemConnected | configuration | item_id unique (UUID) | `plaid_commands.rs:40` | NONE | none |
| PlaidItemDisconnected | idempotent-toggle | item exists | `plaid_commands.rs:609` | NONE | idempotent |
| PlaidAccountMapped | configuration | item exists | `plaid_commands.rs:70` | MED | last-writer-wins / re-check |
| PlaidAccountUnmapped | idempotent-toggle | mapping exists | `plaid_commands.rs:103` | NONE | idempotent |
| PlaidTransactionsSynced | append-commutative | — (entry dedup via PK) | dedup `plaid_commands.rs:158` (PK-backed) | NONE | rely on PK for entry dedup |
| EventServiceRegistered | configuration | service_id unique (UUID) | `event_service_commands.rs:131` | NONE | none |
| EventServiceRemoved | idempotent-toggle | — | `event_service_commands.rs:159` | NONE | idempotent |
| EventServiceSynced | append-commutative | — (`col = col + ?`) | `event_service_commands.rs:401`; staged dedup UNIQUE (`migrations.rs:404`) | NONE | atomic SQL increment |
| BillReceived | stateful-exclusive/seq | amount>0 (local); ingest ref dedup | `validation.rs:290`; ref dedup `ingest_commands.rs:318` (no unique index) | **HIGH** (ingest path) | UNIQUE index on ref; dedup in txn |
| BillPaymentApplied | stateful-exclusive | cumulative payments ≤ amount; not void/paid | `bill_commands.rs:147` read-then-decide | **HIGH** | `UPDATE … WHERE amount_paid+?<=amount` in txn |
| BillVoided | stateful-exclusive | not void AND amount_paid=0 | `bill_commands.rs:218` | **HIGH** | re-check no-payments under head_seq |
| InvoiceIssued | sequence-allocating | amount>0 (local); **intended gapless number — NOT implemented** (UUID) | `validation.rs:325`; `invoice_commands.rs:67` (UUID, no numbering) | NONE (as written) | allocate invoice number in txn *if* gapless numbering is added |
| InvoicePaymentReceived | stateful-exclusive | cumulative payments ≤ amount; not void/paid | `invoice_commands.rs:139` | **HIGH** | conditional UPDATE guard in txn |
| InvoiceVoided | stateful-exclusive | not void AND amount_paid=0 | `invoice_commands.rs:218` | **HIGH** | re-check no-payments under head_seq |
| ReconciliationStarted | stateful-exclusive | **≤1 in-progress per account — NOT enforced at all** | `reconciliation_commands.rs:80` (only checks account exists) | **HIGH** | partial UNIQUE index `(account_id) WHERE status='in_progress'` |
| TransactionCleared | stateful-exclusive | recon in-progress; line not already cleared | `reconciliation_commands.rs:122` (dup via PK `migrations.rs:189`) | MED | re-check status under head_seq; PK covers same-recon dup |
| TransactionUncleared | idempotent-toggle | recon in-progress; line cleared | `reconciliation_commands.rs:189` | MED | idempotent / re-check |
| ReconciliationCompleted | barrier-fence | in-progress; `difference` snapshot of cleared-set | `reconciliation_commands.rs:243` reads cleared_total+balance then appends | **HIGH** | freeze recon + recompute difference under head_seq |
| ReconciliationAbandoned | idempotent-toggle | recon in-progress | `reconciliation_commands.rs:311` | MED | re-check status under head_seq |
| BusinessProfileSet | configuration | singleton header; EIN `NN-NNNNNNN`; NAICS six digits | singleton by `CHECK (id = 'default')` (`023_partnership.sql:6`); shapes `partnership_commands.rs:133` pure + `validation.rs` | NONE | last-writer-wins — **done** (`sync/commands/partnership.rs:122`) |
| PartnerAdmitted | configuration | id must not already exist; `start_date` defaults to the header's formation date | id `partnership_commands.rs:232` in-txn; date read in the **same** txn | MED (date) / **trust** (id) | server mints the id **and** in-txn refuses a taken one — **done** (`sync/commands/partnership.rs:187`) |
| PartnerDetailsUpdated | configuration | partner exists | `partnership_commands.rs:281` in-txn | MED | re-check exists under head_seq; last-writer-wins on the record — **done** (`sync/commands/partnership.rs:249`) |
| PartnerWithdrawn | stateful-exclusive | partner exists AND has not already left | `partnership_commands.rs:310` in-txn | **HIGH** | re-check `end_date IS NULL` under head_seq — **done** (`sync/commands/partnership.rs:293`) |

### Note on the partnership variants

Three of the four are ordinary configuration; `PartnerWithdrawn` is the one with
teeth. Two things are worth more than their table row:

**`PartnerAdmitted`'s id is a trust problem, not a concurrency one.** The `SW-dep`
column asks whether two concurrent writers break the invariant, and for a UUID the
answer is no. The exposure is different in kind: the projector writes
`INSERT OR REPLACE INTO partners (id, …)`, so *any* caller who names an existing id
replaces that partner's name, dates and shares. Nothing rejects it, nothing logs an
anomaly, and the first sign is a K-1 allocating somebody else's income. Hence two
independent locks — the server mints the id (`sync/commands/partnership.rs`), and
`build_admit_partner_in_txn` refuses an id that is already taken regardless of who
minted it.

**`PartnerDetailsUpdated`'s exists-check is not a formality.** The projector's
`UPDATE partners SET … WHERE id = ?1` matches no rows for an id nobody has, so
without the in-txn check the append *succeeds*, the log gains an event, and nothing
changes — a write that reports success and did nothing. That is worse than a
refusal, because a client has no way to tell the difference.

**What is deliberately not in the log at all:** partner taxpayer identification
numbers. They live in `partner_tins` (migration 023), a local table that is not a
projection of any event, for the reason that keeps event-service API keys out —
this log reaches every member's laptop and every backup, permanently. No
partnership event has a TIN field and no sync request accepts one.

## Latent risks, ranked (the ones that will actually bite)

These are `SW-dep = HIGH` **or** already-broken-single-writer. Fix order for the
server append path:

1. **Period-close fence — `JournalEntryPosted` vs `PeriodClosed`**
   (`entry_commands.rs:160`). The marquee TOCTOU, and its writer half
   (`PeriodClosed`) doesn't even exist yet. Build the fence *inside* the append
   transaction against `expected_head_seq` from the start.
2. **Payment over-application — `BillPaymentApplied` / `InvoicePaymentReceived`**
   (`bill_commands.rs:147`, `invoice_commands.rs:139`). Two concurrent partial
   payments each read the same remaining balance and both apply → overpay.
   Replace the read-then-decide with a conditional `UPDATE … WHERE
   amount_paid + ? <= amount`.
3. **Void vs payment — `BillVoided` / `InvoiceVoided`** (`bill_commands.rs:218`,
   `invoice_commands.rs:218`). "no payments applied" read races a payment that
   lands right after.
4. **`account_number` uniqueness + `MAX+1` allocation** (`account_commands.rs:43`,
   `:124`, `:197`). Only a **non-unique** index exists. Add a UNIQUE index and
   allocate the next number inside the append transaction.
5. **Reference-based idempotency has no unique index**
   (`ingest_commands.rs:318`) for `JournalEntryPosted` / `BillReceived`.
   Concurrent re-syncs of the same source double-post. (Plaid and event-service
   paths are safe — they're PK-backed.)
6. **`AccountDeactivated` zero-balance guard** (`account_commands.rs:331`) races
   a concurrent posting to the same account.
7. **`ReconciliationStarted` single-in-progress rule** is **enforced nowhere**
   (`reconciliation_commands.rs:80`) — already violable single-writer. Add a
   partial UNIQUE index.
8. **`ReconciliationCompleted` difference snapshot**
   (`reconciliation_commands.rs:243`) is computed from a non-atomic read of the
   cleared set; a concurrent `TransactionCleared` makes the stored number wrong.
9. **`InvoiceIssued` has no gapless/sequential number** at all — invoice
   identity is a random UUID. Statutory gapless numbering, if required, is a
   sequence-allocating event and must be allocated inside the append txn. (Cross-
   ref ROADMAP investigation #4: jurisdictions differ on gapless requirements.)

## What Phase 1 must build (implied by the table)

- **`expected_head_seq` compare-and-append** on the append path — the single
  primitive every HIGH fix depends on (SPEC §6.2). Spike is Phase-0's other
  half.
- **A real per-append transaction** wrapping validate → invariant-recheck →
  insert, so the recheck and the append commit together.
- **Missing unique/partial indexes**: `accounts.account_number`,
  `journal_entries.reference`, `reconciliations (account_id) WHERE
  status='in_progress'`.
- **Server-side sequence allocation** for any numbered event
  (`InvoiceIssued`/`BillReceived` if numbered) — allocated inside the txn.
- Note: with **Postgres per instance** (SPEC §4.2 [DECIDED]) these transactions
  and partial indexes are straightforward; the SQLite → Postgres port is the
  gating Phase-1 task.
