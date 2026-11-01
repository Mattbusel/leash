#![allow(unused_must_use)]

use super::*;
use near_sdk::test_utils::{get_logs, VMContextBuilder};
use near_sdk::{testing_env, PromiseResult, RuntimeFeesConfig};

const NS_MIN: u64 = 60_000_000_000;

fn wallet() -> AccountId {
    "wallet.testnet".parse().unwrap()
}
fn owner() -> AccountId {
    "owner.testnet".parse().unwrap()
}
fn vendor() -> AccountId {
    "vendor.testnet".parse().unwrap()
}
fn attacker() -> AccountId {
    "drainer.testnet".parse().unwrap()
}
fn registry() -> AccountId {
    "registry.testnet".parse().unwrap()
}
fn near(milli: u128) -> NearToken {
    NearToken::from_millinear(milli)
}

fn agent_pk() -> PublicKey {
    "ed25519:6E8sCci9badyRkXb3JoRpBj5p8C6Tw41ELDZoiihKEtp".parse().unwrap()
}

fn context(predecessor: AccountId, ts: u64) -> VMContextBuilder {
    let mut b = VMContextBuilder::new();
    b.current_account_id(wallet())
        .predecessor_account_id(predecessor.clone())
        .signer_account_id(predecessor)
        .signer_account_pk(agent_pk())
        .account_balance(NearToken::from_near(10))
        .block_timestamp(ts);
    b
}

fn as_agent(ts: u64) {
    testing_env!(context(wallet(), ts).build());
}
fn as_owner(ts: u64) {
    testing_env!(context(owner(), ts).build());
}
fn as_callback(ts: u64, result: PromiseResult) {
    testing_env!(
        context(wallet(), ts).build(),
        near_sdk::test_vm_config(),
        RuntimeFeesConfig::test(),
        Default::default(),
        vec![result]
    );
}

fn registry_ok() -> PromiseResult {
    PromiseResult::Successful(
        br#"{"account_id":"x.testnet","blocked":false,"blocked_by":null,"reports":0,"threshold":2,"contract_code_hash":null}"#.to_vec(),
    )
}

fn base_policy() -> Policy {
    Policy {
        per_payment_cap: near(100),
        daily_cap: near(500),
        per_receiver_daily_cap: near(300),
        max_payments_per_hour: 50,
        min_purpose_len: 8,
        registry: None,
        ..Policy::default()
    }
}

fn new_wallet(policy: Policy) -> LeashWallet {
    as_owner(1);
    LeashWallet::new(owner(), Some(policy))
}

fn status(o: &PromiseOrValue<PayOutcome>) -> (String, Option<String>) {
    match o {
        PromiseOrValue::Value(v) => (v.status.clone(), v.reason.clone()),
        PromiseOrValue::Promise(_) => ("checking".into(), None),
    }
}

/// Run a full successful payment: pay, then the transfer callback.
fn settle(c: &mut LeashWallet, ts: u64, to: AccountId, milli: u128) -> PayOutcome {
    as_agent(ts);
    let out = c.pay(to, near(milli), "invoice 1234 for API credits".into());
    assert_eq!(status(&out).0, "checking", "expected the payment to go async");
    let id = c.next_id - 1;
    as_callback(ts, PromiseResult::Successful(vec![]));
    c.on_transfer_done(U64(id))
}

#[test]
fn init_sets_owner_and_policy() {
    let c = new_wallet(base_policy());
    assert_eq!(c.get_policy(), base_policy());
    let s = c.get_state();
    assert_eq!(s.owner_id, owner());
    assert!(!s.frozen);
    assert_eq!(s.pending_count, 0);
}

#[test]
fn small_payment_settles_and_is_recorded() {
    let mut c = new_wallet(base_policy());
    let out = settle(&mut c, 10, vendor(), 50);
    assert_eq!(out.status, "paid");
    let spend = c.get_spend();
    assert_eq!(spend.spent_24h, near(50));
    assert_eq!(spend.remaining_24h, near(450));
    assert_eq!(spend.in_flight, near(0));
    let h = c.get_history(None);
    assert_eq!(h[0].status, "paid");
    assert_eq!(h[0].receiver_id, vendor());
    assert_eq!(c.get_state().paid_count.0, 1);
    assert!(get_logs().iter().any(|l| l.starts_with("EVENT_JSON:") && l.contains("payment_paid")));
}

#[test]
#[should_panic(expected = "ERR_NOT_AGENT")]
fn pay_rejects_other_callers() {
    let mut c = new_wallet(base_policy());
    testing_env!(context(attacker(), 5).build());
    c.pay(attacker(), near(1), "please give me money".into());
}

#[test]
#[should_panic(expected = "ERR_NOT_AGENT")]
fn owner_account_cannot_call_pay_directly() {
    let mut c = new_wallet(base_policy());
    as_owner(5);
    c.pay(vendor(), near(1), "owner should use withdraw".into());
}

#[test]
#[should_panic(expected = "ERR_NOT_OWNER")]
fn agent_cannot_set_policy() {
    let mut c = new_wallet(base_policy());
    as_agent(5);
    let mut p = base_policy();
    p.per_payment_cap = near(500);
    c.set_policy(p);
}

#[test]
#[should_panic(expected = "ERR_NOT_OWNER")]
fn agent_cannot_withdraw() {
    let mut c = new_wallet(base_policy());
    as_agent(5);
    c.withdraw(near(100), Some(attacker()));
}

#[test]
#[should_panic(expected = "ERR_NOT_OWNER")]
fn agent_cannot_approve_its_own_request() {
    let mut c = new_wallet(base_policy());
    as_agent(5);
    c.pay(vendor(), near(200), "large invoice for GPU time".into());
    c.approve(U64(1));
}

#[test]
#[should_panic(expected = "ERR_NOT_OWNER")]
fn agent_cannot_add_keys() {
    let mut c = new_wallet(base_policy());
    as_agent(5);
    c.add_agent_key(agent_pk(), near(250));
}

#[test]
#[should_panic(expected = "ERR_NOT_OWNER")]
fn agent_cannot_unfreeze() {
    let mut c = new_wallet(base_policy());
    as_owner(5);
    c.freeze(None);
    as_agent(6);
    c.unfreeze();
}

#[test]
fn per_payment_cap_creates_pending() {
    let mut c = new_wallet(base_policy());
    as_agent(5);
    let out = c.pay(vendor(), near(150), "large invoice for GPU time".into());
    assert_eq!(status(&out), ("pending".into(), Some("OVER_PER_PAYMENT_CAP".into())));
    let p = c.get_pending(None, None);
    assert_eq!(p.len(), 1);
    assert_eq!(p[0].amount, near(150));
    assert_eq!(c.get_spend().spent_24h, near(0), "pending does not count as spent");
    assert!(get_logs().iter().any(|l| l.contains("payment_pending")));
}

#[test]
fn daily_cap_creates_pending() {
    let mut p = base_policy();
    p.per_receiver_daily_cap = near(500);
    let mut c = new_wallet(p);
    for i in 0..5 {
        assert_eq!(settle(&mut c, 10 + i, vendor(), 100).status, "paid");
    }
    as_agent(100);
    let out = c.pay(vendor(), near(1), "one more small API call".into());
    assert_eq!(status(&out), ("pending".into(), Some("OVER_DAILY_CAP".into())));
}

#[test]
fn per_receiver_cap_is_separate_from_daily_cap() {
    let mut c = new_wallet(base_policy());
    for i in 0..3 {
        assert_eq!(settle(&mut c, 10 + i, vendor(), 100).status, "paid");
    }
    as_agent(50);
    let out = c.pay(vendor(), near(10), "fourth invoice same vendor".into());
    assert_eq!(status(&out), ("pending".into(), Some("OVER_RECEIVER_DAILY_CAP".into())));
    // A different receiver still has room under the daily cap.
    let other: AccountId = "other-vendor.testnet".parse().unwrap();
    assert_eq!(settle(&mut c, 60, other, 100).status, "paid");
}

#[test]
fn rolling_window_frees_budget_after_24h() {
    let mut c = new_wallet(base_policy());
    for i in 0..3 {
        settle(&mut c, 10 + i, vendor(), 100);
    }
    as_agent(1_000);
    let out = c.pay(vendor(), near(50), "early retry same vendor".into());
    assert_eq!(status(&out).0, "pending");
    // One nanosecond before the window ends the old spends still count.
    let day = DAY_NS;
    as_agent(10 + day - 1);
    assert_eq!(c.get_spend().spent_24h, near(300));
    // After the window, the first payments have aged out.
    let out = settle(&mut c, 13 + day, vendor(), 100);
    assert_eq!(out.status, "paid");
    assert_eq!(c.get_spend().spent_24h, near(100));
}

#[test]
fn rate_limit_blocks_and_recovers() {
    let mut p = base_policy();
    p.max_payments_per_hour = 3;
    let mut c = new_wallet(p);
    for i in 0..3 {
        settle(&mut c, 10 + i * NS_MIN, vendor(), 10);
    }
    as_agent(20 * NS_MIN);
    let out = c.pay(vendor(), near(10), "fourth payment this hour".into());
    assert_eq!(status(&out), ("blocked".into(), Some("RATE_LIMIT".into())));
    // Blocked attempts also count, so a looping agent stays throttled.
    assert_eq!(c.get_spend().attempts_last_hour, 4);
    let out = settle(&mut c, 10 + 61 * NS_MIN, vendor(), 10);
    assert_eq!(out.status, "paid");
}

#[test]
fn denylist_blocks_even_requests() {
    let mut p = base_policy();
    p.denylist = vec![attacker()];
    let mut c = new_wallet(p);
    as_agent(5);
    let out = c.pay(attacker(), near(1), "invoice 9921 urgent payment".into());
    assert_eq!(status(&out), ("blocked".into(), Some("DENYLISTED".into())));
    let r = c.request_payment(attacker(), near(1), "invoice 9921 urgent payment".into());
    assert_eq!(r.status, "blocked");
    assert_eq!(r.reason.as_deref(), Some("DENYLISTED"));
    assert_eq!(c.get_pending(None, None).len(), 0);
    let log = get_logs().join("\n");
    assert!(log.contains("\"event\":\"payment_blocked\""));
    assert!(log.contains("\"reason\":\"DENYLISTED\""));
    assert!(log.contains("\"standard\":\"leash\""));
}

#[test]
fn allowlist_mode_blocks_strangers() {
    let mut p = base_policy();
    p.allowlist_mode = true;
    p.allowlist = vec![vendor()];
    let mut c = new_wallet(p);
    as_agent(5);
    let out = c.pay(attacker(), near(1), "invoice 9921 urgent payment".into());
    assert_eq!(status(&out), ("blocked".into(), Some("NOT_ALLOWLISTED".into())));
    assert_eq!(settle(&mut c, 6, vendor(), 10).status, "paid");
}

#[test]
fn known_contract_needs_allowlist() {
    let dex: AccountId = "dex.testnet".parse().unwrap();
    let mut p = base_policy();
    p.known_contracts = vec![dex.clone()];
    let mut c = new_wallet(p.clone());
    as_agent(5);
    let out = c.pay(dex.clone(), near(1), "swap fee for the router".into());
    assert_eq!(status(&out), ("blocked".into(), Some("CONTRACT_NOT_ALLOWLISTED".into())));
    p.allowlist = vec![dex.clone()];
    as_owner(6);
    c.set_policy(p);
    assert_eq!(settle(&mut c, 7, dex, 10).status, "paid");
}

#[test]
fn purpose_is_required_and_bounded() {
    let mut c = new_wallet(base_policy());
    as_agent(5);
    let out = c.pay(vendor(), near(1), "  pay  ".into());
    assert_eq!(status(&out), ("blocked".into(), Some("PURPOSE_REQUIRED".into())));
    let out = c.pay(vendor(), near(1), "x".repeat(300));
    assert_eq!(status(&out), ("blocked".into(), Some("PURPOSE_TOO_LONG".into())));
    let out = c.pay(vendor(), near(0), "invoice 1234 for API credits".into());
    assert_eq!(status(&out), ("blocked".into(), Some("ZERO_AMOUNT".into())));
}

#[test]
fn self_payment_is_blocked() {
    let mut c = new_wallet(base_policy());
    as_agent(5);
    let out = c.pay(wallet(), near(1), "invoice 1234 for API credits".into());
    assert_eq!(status(&out), ("blocked".into(), Some("SELF_PAYMENT".into())));
}

#[test]
fn freeze_blocks_everything_until_owner_unfreezes() {
    let guardian: AccountId = "guardian.testnet".parse().unwrap();
    let mut p = base_policy();
    p.guardians = vec![guardian.clone()];
    let mut c = new_wallet(p);
    testing_env!(context(guardian, 5).build());
    c.freeze(Some("suspicious invoice".into()));
    assert!(c.get_state().frozen);
    as_agent(6);
    let out = c.pay(vendor(), near(1), "invoice 1234 for API credits".into());
    assert_eq!(status(&out), ("blocked".into(), Some("FROZEN".into())));
    let r = c.request_payment(vendor(), near(1), "invoice 1234 for API credits".into());
    assert_eq!(r.reason.as_deref(), Some("FROZEN"));
    as_owner(7);
    c.unfreeze();
    assert_eq!(settle(&mut c, 8, vendor(), 10).status, "paid");
}

#[test]
#[should_panic(expected = "ERR_NOT_OWNER_OR_GUARDIAN")]
fn strangers_cannot_freeze() {
    let mut c = new_wallet(base_policy());
    testing_env!(context(attacker(), 5).build());
    c.freeze(None);
}

#[test]
fn auto_freeze_after_repeated_blocks() {
    let mut p = base_policy();
    p.denylist = vec![attacker()];
    p.auto_freeze_after_blocks = 3;
    let mut c = new_wallet(p);
    for i in 0..3 {
        as_agent(10 + i);
        c.pay(attacker(), near(1), "invoice 9921 urgent payment".into());
    }
    let s = c.get_state();
    assert!(s.frozen);
    assert_eq!(s.frozen_reason.as_deref(), Some("AUTO_FREEZE_TOO_MANY_BLOCKS"));
    as_agent(20);
    let out = c.pay(vendor(), near(1), "invoice 1234 for API credits".into());
    assert_eq!(status(&out).1.as_deref(), Some("FROZEN"));
}

#[test]
fn owner_approves_pending_and_it_settles() {
    let mut c = new_wallet(base_policy());
    as_agent(5);
    c.pay(vendor(), near(250), "annual license, needs approval".into());
    assert_eq!(c.get_pending(None, None).len(), 1);
    as_owner(6);
    let out = c.approve(U64(1));
    assert_eq!(status(&out).0, "checking");
    assert_eq!(c.get_pending(None, None).len(), 0);
    assert_eq!(c.get_spend().in_flight, near(250));
    as_callback(7, PromiseResult::Successful(vec![]));
    let done = c.on_transfer_done(U64(1));
    assert_eq!(done.status, "paid");
    let h = c.get_history(Some(3));
    assert_eq!(h[0].status, "paid");
    assert_eq!(h[0].reason.as_deref(), Some("OWNER_APPROVED"));
    assert_eq!(h[1].status, "approved");
    assert_eq!(h[2].status, "pending");
    // Approved spend counts toward the rolling cap.
    assert_eq!(c.get_spend().spent_24h, near(250));
}

#[test]
fn owner_rejects_pending() {
    let mut c = new_wallet(base_policy());
    as_agent(5);
    let r = c.request_payment(vendor(), near(20), "please approve this one".into());
    assert_eq!(r.status, "pending");
    as_owner(6);
    c.reject(U64(1), Some("not needed".into()));
    assert!(c.get_pending(None, None).is_empty());
    assert_eq!(c.get_history(Some(1))[0].status, "rejected");
}

#[test]
#[should_panic(expected = "ERR_FROZEN")]
fn approve_refused_while_frozen() {
    let mut c = new_wallet(base_policy());
    as_agent(5);
    c.pay(vendor(), near(250), "annual license, needs approval".into());
    as_owner(6);
    c.freeze(None);
    c.approve(U64(1));
}

#[test]
fn pending_queue_is_bounded() {
    let mut p = base_policy();
    p.max_payments_per_hour = 100;
    let mut c = new_wallet(p);
    for i in 0..MAX_PENDING as u64 {
        as_agent(10 + i);
        c.request_payment(vendor(), near(1), "please approve this one".into());
    }
    as_agent(100);
    let r = c.request_payment(vendor(), near(1), "please approve this one".into());
    assert_eq!(r.reason.as_deref(), Some("PENDING_QUEUE_FULL"));
}

#[test]
fn insufficient_funds_blocks() {
    let mut p = base_policy();
    p.per_payment_cap = NearToken::from_near(20);
    p.daily_cap = NearToken::from_near(50);
    p.per_receiver_daily_cap = NearToken::from_near(50);
    let mut c = new_wallet(p);
    as_agent(5);
    let out = c.pay(vendor(), NearToken::from_near(10), "invoice larger than balance".into());
    assert_eq!(status(&out), ("blocked".into(), Some("INSUFFICIENT_FUNDS".into())));
}

#[test]
fn registry_block_rolls_back_reservation() {
    let mut p = base_policy();
    p.registry = Some(registry());
    let mut c = new_wallet(p);
    as_agent(5);
    let out = c.pay(attacker(), near(50), "invoice 9921 urgent payment".into());
    assert_eq!(status(&out).0, "checking");
    assert_eq!(c.get_spend().spent_24h, near(50), "reserved while checking");
    as_callback(
        6,
        PromiseResult::Successful(
            br#"{"account_id":"drainer.testnet","blocked":true,"blocked_by":"reports","reports":2,"threshold":2,"contract_code_hash":null}"#.to_vec(),
        ),
    );
    let res = c.on_registry_checked(U64(1));
    assert_eq!(status(&res), ("blocked".into(), Some("BLOCKLISTED:reports".into())));
    let s = c.get_spend();
    assert_eq!(s.spent_24h, near(0));
    assert_eq!(s.in_flight, near(0));
    assert_eq!(c.get_history(Some(1))[0].reason.as_deref(), Some("BLOCKLISTED:reports"));
}

#[test]
fn registry_ok_schedules_transfer() {
    let mut p = base_policy();
    p.registry = Some(registry());
    let mut c = new_wallet(p);
    as_agent(5);
    c.pay(vendor(), near(50), "invoice 1234 for API credits".into());
    as_callback(6, registry_ok());
    let res = c.on_registry_checked(U64(1));
    assert_eq!(status(&res).0, "checking", "a transfer promise is returned");
    as_callback(7, PromiseResult::Successful(vec![]));
    assert_eq!(c.on_transfer_done(U64(1)).status, "paid");
    assert_eq!(c.get_spend().spent_24h, near(50));
}

#[test]
fn registry_failure_fails_closed() {
    let mut p = base_policy();
    p.registry = Some(registry());
    let mut c = new_wallet(p);
    as_agent(5);
    c.pay(vendor(), near(50), "invoice 1234 for API credits".into());
    as_callback(6, PromiseResult::Failed);
    let res = c.on_registry_checked(U64(1));
    assert_eq!(status(&res), ("blocked".into(), Some("REGISTRY_UNAVAILABLE".into())));
    assert_eq!(c.get_spend().in_flight, near(0));
}

#[test]
fn registry_garbage_fails_closed() {
    let mut p = base_policy();
    p.registry = Some(registry());
    let mut c = new_wallet(p);
    as_agent(5);
    c.pay(vendor(), near(50), "invoice 1234 for API credits".into());
    as_callback(6, PromiseResult::Successful(b"not json".to_vec()));
    let res = c.on_registry_checked(U64(1));
    assert_eq!(status(&res).1.as_deref(), Some("REGISTRY_BAD_RESPONSE"));
}

#[test]
fn registry_attested_contract_needs_allowlist() {
    let mut p = base_policy();
    p.registry = Some(registry());
    let mut c = new_wallet(p);
    as_agent(5);
    c.pay(vendor(), near(50), "invoice 1234 for API credits".into());
    as_callback(
        6,
        PromiseResult::Successful(
            br#"{"blocked":false,"reports":0,"contract_code_hash":"8tY2kNq3"}"#.to_vec(),
        ),
    );
    let res = c.on_registry_checked(U64(1));
    assert_eq!(status(&res).1.as_deref(), Some("CONTRACT_NOT_ALLOWLISTED"));
}

#[test]
fn freeze_between_call_and_callback_stops_transfer() {
    let mut p = base_policy();
    p.registry = Some(registry());
    let mut c = new_wallet(p);
    as_agent(5);
    c.pay(vendor(), near(50), "invoice 1234 for API credits".into());
    as_owner(6);
    c.freeze(Some("owner saw something".into()));
    as_callback(7, registry_ok());
    let res = c.on_registry_checked(U64(1));
    assert_eq!(status(&res).1.as_deref(), Some("FROZEN"));
    assert_eq!(c.get_spend().in_flight, near(0));
}

#[test]
fn failed_transfer_rolls_back() {
    let mut c = new_wallet(base_policy());
    as_agent(5);
    c.pay("ghost.testnet".parse().unwrap(), near(50), "invoice to a missing account".into());
    assert_eq!(c.get_spend().spent_24h, near(50));
    as_callback(6, PromiseResult::Failed);
    let out = c.on_transfer_done(U64(1));
    assert_eq!(out.status, "failed");
    let s = c.get_spend();
    assert_eq!(s.spent_24h, near(0));
    assert_eq!(s.in_flight, near(0));
    assert_eq!(c.get_state().paid_count.0, 0);
}

#[test]
fn history_is_a_bounded_ring() {
    let mut p = base_policy();
    p.max_payments_per_hour = 1_000;
    p.denylist = vec![attacker()];
    let mut c = new_wallet(p);
    for i in 0..(HISTORY_CAP as u64 + 25) {
        as_agent(10 + i);
        c.pay(attacker(), near(1), format!("attempt number {i:04}"));
    }
    let h = c.get_history(None);
    assert_eq!(h.len(), HISTORY_CAP as usize);
    assert_eq!(h[0].purpose, format!("attempt number {:04}", HISTORY_CAP + 24));
    assert_eq!(h.last().unwrap().purpose, "attempt number 0025");
    assert_eq!(c.get_state().total_decisions.0, HISTORY_CAP as u64 + 25);
    assert_eq!(c.get_history(Some(5)).len(), 5);
}

#[test]
#[should_panic(expected = "ERR_PER_PAYMENT_CAP_ABOVE_DAILY_CAP")]
fn policy_validation() {
    let mut p = base_policy();
    p.per_payment_cap = near(600);
    new_wallet(p);
}

#[test]
fn withdraw_by_owner() {
    let mut c = new_wallet(base_policy());
    as_owner(5);
    c.withdraw(near(1000), None);
    assert!(get_logs().iter().any(|l| l.contains("withdrawn")));
}
