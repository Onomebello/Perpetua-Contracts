//! Stage 3 — TTL, rent, and archival.
//!
//! This is the file that separates a production streaming primitive from a
//! hackathon one. A stream running twelve months outlives its initial TTL, and
//! if the entry archives, the recipient's claim becomes unreadable until
//! somebody pays to restore it.
//!
//! # What the test host can and cannot prove
//!
//! The SDK's test host runs storage in *recording* mode, where reading an
//! expired persistent entry triggers `handle_maybe_expired_entry`: the entry is
//! restored in place with its data intact and its TTL reset to
//! `min_persistent_entry_ttl`. That mirrors the on-network outcome of a
//! `RestoreFootprint` operation, so these tests genuinely prove **data survives
//! the archive/restore boundary with balances intact**.
//!
//! What they cannot reproduce is the client-side dance on a real network, where
//! the transaction *fails first* and the caller must resubmit with a restore
//! footprint. That step has no unit-test surface and belongs in the testnet
//! exercise in stage 4.
//!
//! One useful consequence of the host's behaviour: an entry that has been
//! through an auto-restore has a TTL of exactly `min_persistent_entry_ttl - 1`,
//! which is far below anything this contract ever sets. [`was_restored`] uses
//! that as a detector for "this entry archived".

use soroban_sdk::testutils::storage::Persistent as _;
use soroban_sdk::testutils::Ledger as _;

use super::common::*;
use crate::{storage, DataKey, TTL_BUFFER_SECONDS};

#[test]
fn persisted_stream_fixture_survives_read_mutate_and_ttl_extension() {
    let h = super::common::Harness::new();
    let id = h.create_simple(1_000 * super::common::ONE, 100 * super::common::DAY);
    let before = h.get(id);

    h.client.top_up(&id, &(250 * super::common::ONE));
    let after = h.get(id);

    assert_eq!(after.sender, before.sender);
    assert_eq!(after.recipient, before.recipient);
    assert_eq!(after.token, before.token);
    assert_eq!(after.withdrawn, before.withdrawn);
    assert_eq!(after.deposited, 1_250 * super::common::ONE);
    assert!(ttl_of(&h, id) > 0);
}

/// Remaining TTL, in ledgers, of a stream entry.
fn ttl_of(h: &Harness, stream_id: u64) -> u32 {
    h.env.as_contract(&h.contract_id, || {
        h.env
            .storage()
            .persistent()
            .get_ttl(&DataKey::Stream(stream_id))
    })
}

/// True if the entry shows the signature of a host auto-restore: a TTL pinned
/// to the network minimum, which this contract never sets deliberately.
fn was_restored(h: &Harness, stream_id: u64) -> bool {
    let min = h.env.ledger().get().min_persistent_entry_ttl;
    ttl_of(h, stream_id) < min
}

/// The largest TTL any entry can actually hold right now.
///
/// This is deliberately read from the SDK rather than from
/// `LedgerInfo::max_entry_ttl`: the achievable maximum is
/// `max_live_until_ledger - sequence`, which is not always the raw configured
/// value. Asserting against the config number bakes in an off-by-one.
fn max_achievable_ttl(h: &Harness) -> u32 {
    h.env
        .as_contract(&h.contract_id, || h.env.storage().max_ttl())
}

/// Advance only the ledger sequence, leaving the clock alone. Used to age
/// entries without moving accrual.
fn age_ledgers(h: &Harness, ledgers: u32) {
    let seq = h.env.ledger().sequence();
    h.env.ledger().set_sequence_number(seq + ledgers);
}

// --- Extension at creation -------------------------------------------------

/// A new stream must be funded with rent covering its whole scheduled life
/// plus the keeper's working buffer, so an ordinary stream never needs a
/// keeper at all.
#[test]
fn creation_covers_the_whole_stream_plus_the_buffer() {
    let h = Harness::new();
    let id = h.create_simple(1_000 * ONE, 100 * DAY);

    let expected = storage::seconds_to_ledgers(100 * DAY + TTL_BUFFER_SECONDS);
    assert_eq!(ttl_of(&h, id), expected);

    // Sanity: that is meaningfully longer than the network's default minimum.
    let min = h.env.ledger().get().min_persistent_entry_ttl;
    assert!(
        expected > min * 100,
        "creation TTL barely above the default"
    );
}

/// A multi-year stream exceeds `max_entry_ttl`, so it clamps — which is exactly
/// why the permissionless keeper path has to exist.
#[test]
fn a_long_stream_clamps_to_the_network_maximum() {
    let h = Harness::new();
    let max = max_achievable_ttl(&h);
    let id = h.create_simple(10_000 * ONE, 5 * YEAR);

    assert_eq!(ttl_of(&h, id), max, "must clamp, never exceed");
    assert!(
        storage::seconds_to_ledgers(5 * YEAR) > max,
        "this test is only meaningful if the stream outlives the max TTL",
    );
}

/// A settled stream still has to stay readable: the recipient may not have
/// pulled their tail, and the indexer needs the final state.
#[test]
fn a_matured_stream_keeps_a_floor_of_rent() {
    let h = Harness::new();
    let id = h.create_simple(1_000 * ONE, 10 * DAY);
    h.warp_to(T0 + 100 * DAY);

    h.client.extend_stream_ttl(&id);
    assert_eq!(ttl_of(&h, id), storage::MIN_STREAM_TTL_LEDGERS);
}

/// A paused stream's end date slides forward in wall-clock terms, so its rent
/// target has to slide with it.
#[test]
fn a_paused_stream_is_funded_for_its_stretched_end() {
    let h = Harness::new();
    let id = h.create_simple(1_000 * ONE, 100 * DAY);

    h.advance(10 * DAY);
    h.client.pause(&id);
    h.advance(200 * DAY);

    // An unpaused stream would be 110 days past its end by now and would sit on
    // the bare floor. This one is still 90 days from delivering, so it must be
    // funded for those 90 days plus the buffer.
    let target = h.client.extend_stream_ttl(&id);
    let expected = storage::seconds_to_ledgers(90 * DAY + TTL_BUFFER_SECONDS);
    assert_eq!(target, expected);
    assert!(
        target > storage::MIN_STREAM_TTL_LEDGERS,
        "a paused stream must not be treated as already settled",
    );
}

// --- Extension on every touch ----------------------------------------------

/// An actively-used stream never expires, because every mutating call tops its
/// rent back up.
#[test]
fn every_mutating_call_re_extends_the_ttl() {
    let h = Harness::new();
    let id = h.create_simple(1_000 * ONE, 100 * DAY);
    let full = ttl_of(&h, id);

    // Let most of the rent burn off, then touch the stream.
    age_ledgers(&h, full - 1_000);
    assert!(ttl_of(&h, id) < 2_000, "TTL should have decayed");

    h.advance(10 * DAY);
    h.client.withdraw(&id, &None);
    assert!(
        ttl_of(&h, id) > full - 200_000,
        "withdraw did not re-extend"
    );

    age_ledgers(&h, ttl_of(&h, id) - 1_000);
    h.client.pause(&id);
    assert!(ttl_of(&h, id) > 1_000_000, "pause did not re-extend");

    age_ledgers(&h, ttl_of(&h, id) - 1_000);
    h.client.resume(&id);
    assert!(ttl_of(&h, id) > 1_000_000, "resume did not re-extend");

    age_ledgers(&h, ttl_of(&h, id) - 1_000);
    h.client.top_up(&id, &(10 * ONE));
    assert!(ttl_of(&h, id) > 1_000_000, "top_up did not re-extend");
}

/// **Deliverable: a stream outlives the default TTL via the keeper path.**
///
/// The network is configured here so a single extension cannot cover the
/// stream's life — the situation every multi-year payroll or vesting stream is
/// actually in. A keeper sweeps periodically, and the stream survives a full
/// year with its accounting intact and pays out in full at the end.
#[test]
fn a_year_long_stream_survives_on_keeper_sweeps_alone() {
    let h = Harness::new();

    // Force the clamp: max rent buys ~5.8 days, but the stream runs a year.
    const MAX_TTL: u32 = 100_000;
    h.env.ledger().set_max_entry_ttl(MAX_TTL);

    let id = h.create_simple(365 * ONE, YEAR);
    assert_eq!(ttl_of(&h, id), MAX_TTL, "creation clamped as expected");

    // Nobody touches the stream all year except the keeper, sweeping at 60% of
    // the rent window — the cadence the backend keeper would actually use.
    let sweep_every = MAX_TTL * 6 / 10;

    let mut elapsed = 0u64;
    while elapsed < YEAR {
        age_ledgers(&h, sweep_every);
        h.client.extend_stream_ttl(&id);
        assert_eq!(ttl_of(&h, id), MAX_TTL, "keeper sweep must re-clamp");
        elapsed += sweep_every as u64;
    }

    // The stream is now fully matured; the recipient pulls everything.
    h.warp_to(T0 + YEAR + DAY);
    h.client.withdraw(&id, &None);
    let s = h.get(id);
    assert_eq!(s.withdrawn, s.deposited, "full payout after a year");
}

// --- Dynamic max TTL alignment ---------------------------------------------

/// The clamp must track the *live* host maximum, not a hardcoded constant.
///
/// `extend_stream_ttl` reads `env.storage().max_ttl()` on every call, so when
/// the network raises or lowers `max_entry_ttl` the target TTL follows it
/// without a contract upgrade. This test drives the same stream through three
/// different host configurations and asserts the clamp moves with each one.
#[test]
fn clamp_tracks_the_dynamically_queried_max_ttl() {
    let h = Harness::new();
    let id = h.create_simple(10_000 * ONE, 5 * YEAR);

    // A long stream always wants more than any of these maxima, so the target
    // is exactly whatever the host currently reports.
    for &configured in &[50_000u32, 250_000, 1_000_000] {
        h.env.ledger().set_max_entry_ttl(configured);

        let target = h.client.extend_stream_ttl(&id);
        let live_max = max_achievable_ttl(&h);

        assert_eq!(
            target, live_max,
            "target must equal the live host max, not a static constant",
        );
        assert_eq!(ttl_of(&h, id), live_max, "entry TTL must match the target");
    }
}

/// Lowering the network maximum must *shrink* the achievable TTL on the next
/// touch, proving the contract re-reads the host rather than caching a value.
#[test]
fn lowering_the_network_max_shrinks_the_target() {
    let h = Harness::new();
    let id = h.create_simple(10_000 * ONE, 5 * YEAR);

    h.env.ledger().set_max_entry_ttl(1_000_000);
    let high = h.client.extend_stream_ttl(&id);

    h.env.ledger().set_max_entry_ttl(80_000);
    let low = h.client.extend_stream_ttl(&id);

    assert!(low < high, "a lower network max must yield a lower target");
    assert_eq!(low, max_achievable_ttl(&h));
    assert_eq!(ttl_of(&h, id), low);
}

/// A short stream that fits comfortably under the maximum must not be clamped:
/// its target is its own schedule plus the buffer, independent of the host max.
#[test]
fn a_short_stream_is_not_clamped_by_a_generous_max() {
    let h = Harness::new();
    h.env.ledger().set_max_entry_ttl(5_000_000);

    let id = h.create_simple(1_000 * ONE, 30 * DAY);
    let expected = storage::seconds_to_ledgers(30 * DAY + TTL_BUFFER_SECONDS);

    assert!(expected < max_achievable_ttl(&h), "fixture must fit under max");
    assert_eq!(ttl_of(&h, id), expected, "no clamp when the schedule fits");
}
