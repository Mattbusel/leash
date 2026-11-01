//! Leash wallet: a NEAR contract account that holds an AI agent's money.
//!
//! The human owner controls policy from their own account. The agent only holds a
//! function-call access key on this contract account, restricted to `pay` and
//! `request_payment`. Every payment goes through the policy below, then through a
//! cross-contract check against the shared blocklist registry, and only then is the
//! NEAR transferred from inside the contract.
//!
//! Decisions are never panics: a blocked or pending payment is recorded in history and
//! emitted as a NEP-297 event so the transaction log always says why.

use near_sdk::json_types::U64;
use near_sdk::serde::{Deserialize, Serialize};
use near_sdk::store::{IterableMap, LookupMap, Vector};
use near_sdk::{
    env, near, require, AccountId, Allowance, BorshStorageKey, Gas, NearToken, PanicOnDefault,
    Promise, PromiseError, PromiseOrValue, PublicKey,
};

/// Gas for the registry `get_status` view, called as a function call.
const GAS_REGISTRY_CHECK: Gas = Gas::from_tgas(5);
/// Gas for `on_registry_checked`, which may schedule a transfer and one more callback.
const GAS_ON_REGISTRY: Gas = Gas::from_tgas(30);
/// Gas for `on_transfer_done`.
const GAS_ON_TRANSFER: Gas = Gas::from_tgas(10);

/// Number of history entries kept on chain (ring buffer).
pub const HISTORY_CAP: u32 = 100;
/// Max open pending requests, so a looping agent cannot fill storage.
pub const MAX_PENDING: u32 = 20;
/// Max entries in each policy list.
pub const MAX_LIST_LEN: usize = 100;
/// Max purpose (memo) length in bytes.
pub const MAX_PURPOSE_LEN: usize = 256;
/// NEAR kept back from payments for storage growth.
pub const STORAGE_RESERVE: NearToken = NearToken::from_millinear(500);

const HOUR_NS: u64 = 3_600_000_000_000;
const DAY_NS: u64 = 24 * HOUR_NS;

/// Methods the agent's function-call key is allowed to call.
pub const AGENT_METHODS: &str = "pay,request_payment";

#[derive(BorshStorageKey)]
#[near]
enum StorageKey {
    Pending,
    InFlight,
    History,
}

/// Spending policy. All amounts are yoctoNEAR strings in JSON.
#[near(serializers = [borsh, json])]
#[derive(Clone, Debug, PartialEq)]
pub struct Policy {
    /// Payments above this become pending requests.
    pub per_payment_cap: NearToken,
    /// Rolling 24 hour cap across all receivers.
    pub daily_cap: NearToken,
    /// Rolling 24 hour cap per receiver.
    pub per_receiver_daily_cap: NearToken,
    /// Max `pay`/`request_payment` attempts in any rolling hour (blocked ones count too).
    pub max_payments_per_hour: u32,
    /// When true, only receivers in `allowlist` can be paid without the owner.
    pub allowlist_mode: bool,
    pub allowlist: Vec<AccountId>,
    /// Never pay these, not even through an approved request.
    pub denylist: Vec<AccountId>,
    /// When true, receivers known to be contracts (owner-marked here, or attested in the
    /// registry) must be in `allowlist`.
    pub contracts_require_allowlist: bool,
    /// Accounts the owner knows are contracts.
    pub known_contracts: Vec<AccountId>,
    /// Minimum purpose (memo) length. 0 disables the rule.
    pub min_purpose_len: u32,
    /// Shared blocklist registry contract checked before every transfer.
    pub registry: Option<AccountId>,
    /// Freeze automatically after this many blocked attempts in an hour. 0 disables.
    pub auto_freeze_after_blocks: u32,
    /// Accounts that may freeze (but not unfreeze) the wallet.
    pub guardians: Vec<AccountId>,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            per_payment_cap: NearToken::from_millinear(100),
            daily_cap: NearToken::from_millinear(500),
            per_receiver_daily_cap: NearToken::from_millinear(300),
            max_payments_per_hour: 20,
            allowlist_mode: false,
            allowlist: vec![],
            denylist: vec![],
            contracts_require_allowlist: true,
            known_contracts: vec![],
            min_purpose_len: 8,
            registry: None,
            auto_freeze_after_blocks: 0,
            guardians: vec![],
        }
    }
}

#[near(serializers = [borsh, json])]
#[derive(Clone, Debug)]
pub struct SpendEntry {
    pub id: U64,
    pub ts: U64,
    pub receiver_id: AccountId,
    pub amount: NearToken,
}

#[near(serializers = [borsh, json])]
#[derive(Clone, Debug)]
pub struct PendingRequest {
    pub id: U64,
    pub created_at: U64,
    pub receiver_id: AccountId,
    pub amount: NearToken,
    pub purpose: String,
    /// Why it needs the owner: a reason code such as OVER_PER_PAYMENT_CAP.
    pub reason: String,
    /// Public key that signed the agent transaction.
    pub signer_pk: String,
}

#[near(serializers = [borsh, json])]
#[derive(Clone, Debug)]
pub struct InFlight {
    pub id: U64,
    pub receiver_id: AccountId,
    pub amount: NearToken,
    pub purpose: String,
    pub approved: bool,
    pub signer_pk: String,
}

#[near(serializers = [borsh, json])]
#[derive(Clone, Debug)]
pub struct HistoryEntry {
    pub id: U64,
    pub ts: U64,
    pub receiver_id: AccountId,
    pub amount: NearToken,
    pub purpose: String,
    /// paid | blocked | pending | approved | rejected | failed
    pub status: String,
    pub reason: Option<String>,
    pub signer_pk: String,
}

/// Result returned to the caller of `pay` and `request_payment`.
#[near(serializers = [json])]
#[derive(Clone, Debug, PartialEq)]
pub struct PayOutcome {
    pub id: U64,
    /// paid | blocked | pending | failed | checking
    pub status: String,
    pub reason: Option<String>,
}

/// The subset of the registry's `get_status` answer the wallet needs.
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(crate = "near_sdk::serde")]
pub struct RegistryStatus {
    pub blocked: bool,
    #[serde(default)]
    pub blocked_by: Option<String>,
    #[serde(default)]
    pub reports: u32,
    #[serde(default)]
    pub contract_code_hash: Option<String>,
}

#[near(serializers = [json])]
pub struct SpendView {
    pub now: U64,
    pub window_ns: U64,
    pub spent_24h: NearToken,
    pub daily_cap: NearToken,
    pub remaining_24h: NearToken,
    pub per_receiver: Vec<(AccountId, NearToken)>,
    pub per_receiver_daily_cap: NearToken,
    pub attempts_last_hour: u32,
    pub max_payments_per_hour: u32,
    pub blocked_last_hour: u32,
    pub in_flight: NearToken,
}

#[near(serializers = [json])]
pub struct StateView {
    pub owner_id: AccountId,
    pub frozen: bool,
    pub frozen_reason: Option<String>,
    pub balance: NearToken,
    pub available: NearToken,
    pub pending_count: u32,
    pub total_decisions: U64,
    pub paid_total: NearToken,
    pub paid_count: U64,
    pub blocked_count: U64,
}

#[near(contract_state)]
#[derive(PanicOnDefault)]
pub struct LeashWallet {
    owner_id: AccountId,
    policy: Policy,
    frozen: bool,
    frozen_reason: Option<String>,
    /// Payments (settled or in flight) inside the last 24 hours.
    spends: Vec<SpendEntry>,
    /// Timestamps of agent attempts inside the last hour.
    attempts: Vec<u64>,
    /// Timestamps of blocked decisions inside the last hour.
    blocked_recent: Vec<u64>,
    pending: IterableMap<u64, PendingRequest>,
    in_flight: LookupMap<u64, InFlight>,
    in_flight_total: NearToken,
    next_id: u64,
    history: Vector<HistoryEntry>,
    history_total: u64,
    paid_total: NearToken,
    paid_count: u64,
    blocked_count: u64,
}

fn emit(event: &str, data: near_sdk::serde_json::Value) {
    let ev = near_sdk::serde_json::json!({
        "standard": "leash",
        "version": "1.0.0",
        "event": event,
        "data": [data],
    });
    env::log_str(&format!("EVENT_JSON:{}", ev));
}

fn add(a: NearToken, b: NearToken) -> NearToken {
    a.checked_add(b).unwrap_or_else(|| env::panic_str("ERR_OVERFLOW"))
}

#[near]
impl LeashWallet {
    #[init]
    pub fn new(owner_id: AccountId, policy: Option<Policy>) -> Self {
        let policy = policy.unwrap_or_default();
        Self::validate_policy(&policy);
        Self {
            owner_id,
            policy,
            frozen: false,
            frozen_reason: None,
            spends: vec![],
            attempts: vec![],
            blocked_recent: vec![],
            pending: IterableMap::new(StorageKey::Pending),
            in_flight: LookupMap::new(StorageKey::InFlight),
            in_flight_total: NearToken::from_yoctonear(0),
            next_id: 1,
            history: Vector::new(StorageKey::History),
            history_total: 0,
            paid_total: NearToken::from_yoctonear(0),
            paid_count: 0,
            blocked_count: 0,
        }
    }

    // ------------------------------------------------------------------
    // Agent methods (callable only through a key on this contract account)
    // ------------------------------------------------------------------

    /// Ask the wallet to pay `receiver_id`. Returns the decision. If the payment passes the
    /// local policy, the result is the outcome of the registry check and the transfer.
    pub fn pay(
        &mut self,
        receiver_id: AccountId,
        amount: NearToken,
        purpose: String,
    ) -> PromiseOrValue<PayOutcome> {
        self.assert_agent();
        let now = env::block_timestamp();
        self.prune(now);
        let id = self.take_id();
        let signer_pk = Self::signer_pk();

        if let Some(reason) = self.precheck(&receiver_id, amount, &purpose, now) {
            self.attempts.push(now);
            return PromiseOrValue::Value(self.record_block(
                id, now, &receiver_id, amount, &purpose, &reason, &signer_pk,
            ));
        }
        self.attempts.push(now);

        if let Some(reason) = self.cap_breach(&receiver_id, amount) {
            return self.make_pending(id, now, receiver_id, amount, purpose, reason, signer_pk);
        }

        if amount > self.available() {
            return PromiseOrValue::Value(self.record_block(
                id, now, &receiver_id, amount, &purpose, "INSUFFICIENT_FUNDS", &signer_pk,
            ));
        }

        self.start_payment(InFlight {
            id: U64(id),
            receiver_id,
            amount,
            purpose,
            approved: false,
            signer_pk,
        })
    }

    /// Ask the owner to approve a payment. Local hard rules (freeze, memo, denylist, rate)
    /// still apply; caps and the allowlist are left to the human.
    pub fn request_payment(
        &mut self,
        receiver_id: AccountId,
        amount: NearToken,
        purpose: String,
    ) -> PayOutcome {
        self.assert_agent();
        let now = env::block_timestamp();
        self.prune(now);
        let id = self.take_id();
        let signer_pk = Self::signer_pk();
        let hard = self
            .hard_rules(&receiver_id, amount, &purpose, now)
            .or_else(|| self.rate_rule());
        self.attempts.push(now);
        if let Some(reason) = hard {
            return self.record_block(id, now, &receiver_id, amount, &purpose, &reason, &signer_pk);
        }
        match self.make_pending(id, now, receiver_id, amount, purpose, "AGENT_REQUEST".into(), signer_pk) {
            PromiseOrValue::Value(v) => v,
            PromiseOrValue::Promise(_) => env::panic_str("ERR_UNREACHABLE"),
        }
    }

    // ------------------------------------------------------------------
    // Owner methods
    // ------------------------------------------------------------------

    pub fn set_policy(&mut self, policy: Policy) {
        self.assert_owner();
        Self::validate_policy(&policy);
        self.policy = policy;
        emit("policy_updated", near_sdk::serde_json::to_value(&self.policy).unwrap());
    }

    /// Approve a pending request. It skips the caps and the allowlist, but still runs the
    /// denylist, the shared blocklist check and the balance check.
    pub fn approve(&mut self, request_id: U64) -> PromiseOrValue<PayOutcome> {
        self.assert_owner();
        require!(!self.frozen, "ERR_FROZEN: unfreeze before approving");
        let req = self
            .pending
            .remove(&request_id.0)
            .unwrap_or_else(|| env::panic_str("ERR_NO_SUCH_REQUEST"));
        require!(
            !self.policy.denylist.contains(&req.receiver_id),
            "ERR_DENYLISTED: remove from denylist first"
        );
        let now = env::block_timestamp();
        self.prune(now);
        require!(req.amount <= self.available(), "ERR_INSUFFICIENT_FUNDS");
        emit(
            "request_approved",
            near_sdk::serde_json::json!({
                "id": req.id, "receiver_id": req.receiver_id, "amount": req.amount,
                "approved_by": env::predecessor_account_id(),
            }),
        );
        self.push_history(HistoryEntry {
            id: req.id,
            ts: U64(now),
            receiver_id: req.receiver_id.clone(),
            amount: req.amount,
            purpose: req.purpose.clone(),
            status: "approved".into(),
            reason: Some(req.reason.clone()),
            signer_pk: req.signer_pk.clone(),
        });
        self.start_payment(InFlight {
            id: req.id,
            receiver_id: req.receiver_id,
            amount: req.amount,
            purpose: req.purpose,
            approved: true,
            signer_pk: req.signer_pk,
        })
    }

    pub fn reject(&mut self, request_id: U64, note: Option<String>) {
        self.assert_owner();
        let req = self
            .pending
            .remove(&request_id.0)
            .unwrap_or_else(|| env::panic_str("ERR_NO_SUCH_REQUEST"));
        let now = env::block_timestamp();
        emit(
            "request_rejected",
            near_sdk::serde_json::json!({
                "id": req.id, "receiver_id": req.receiver_id, "amount": req.amount, "note": note,
            }),
        );
        self.push_history(HistoryEntry {
            id: req.id,
            ts: U64(now),
            receiver_id: req.receiver_id,
            amount: req.amount,
            purpose: req.purpose,
            status: "rejected".into(),
            reason: note.or(Some(req.reason)),
            signer_pk: req.signer_pk,
        });
    }

    /// Kill switch. The owner or any guardian can freeze.
    pub fn freeze(&mut self, reason: Option<String>) {
        let caller = env::predecessor_account_id();
        require!(
            caller == self.owner_id || self.policy.guardians.contains(&caller),
            "ERR_NOT_OWNER_OR_GUARDIAN"
        );
        self.set_frozen(reason.unwrap_or_else(|| "MANUAL".into()), caller);
    }

    /// Only the owner can unfreeze.
    pub fn unfreeze(&mut self) {
        self.assert_owner();
        self.frozen = false;
        self.frozen_reason = None;
        self.blocked_recent.clear();
        emit("unfrozen", near_sdk::serde_json::json!({ "by": self.owner_id }));
    }

    /// Move NEAR out of the wallet. Goes to the owner unless `to` is given.
    pub fn withdraw(&mut self, amount: NearToken, to: Option<AccountId>) -> Promise {
        self.assert_owner();
        require!(amount.as_yoctonear() > 0, "ERR_ZERO_AMOUNT");
        require!(amount <= self.available(), "ERR_INSUFFICIENT_FUNDS");
        let to = to.unwrap_or_else(|| self.owner_id.clone());
        emit(
            "withdrawn",
            near_sdk::serde_json::json!({ "to": to, "amount": amount }),
        );
        Promise::new(to).transfer(amount)
    }

    /// Add a function-call key for an agent. It can only call `pay` and `request_payment`
    /// on this contract, cannot attach deposits, and its gas spend is capped by `allowance`.
    pub fn add_agent_key(&mut self, public_key: PublicKey, allowance: NearToken) -> Promise {
        self.assert_owner();
        let allowance = Allowance::limited(allowance)
            .unwrap_or_else(|| env::panic_str("ERR_ZERO_ALLOWANCE"));
        emit(
            "agent_key_added",
            near_sdk::serde_json::json!({ "public_key": public_key, "methods": AGENT_METHODS }),
        );
        Promise::new(env::current_account_id()).add_access_key_allowance(
            public_key,
            allowance,
            env::current_account_id(),
            AGENT_METHODS,
        )
    }

    /// Revoke an agent key (or any key on this account).
    pub fn remove_agent_key(&mut self, public_key: PublicKey) -> Promise {
        self.assert_owner();
        emit(
            "agent_key_removed",
            near_sdk::serde_json::json!({ "public_key": public_key }),
        );
        Promise::new(env::current_account_id()).delete_key(public_key)
    }

    // ------------------------------------------------------------------
    // Callbacks
    // ------------------------------------------------------------------

    #[private]
    pub fn on_registry_checked(&mut self, payment_id: U64) -> PromiseOrValue<PayOutcome> {
        let flight = self
            .in_flight
            .get(&payment_id.0)
            .cloned()
            .unwrap_or_else(|| env::panic_str("ERR_UNKNOWN_PAYMENT"));
        let now = env::block_timestamp();

        let status: Result<RegistryStatus, &str> = match env::promise_result_checked(0, 4096) {
            Ok(bytes) => near_sdk::serde_json::from_slice::<RegistryStatus>(&bytes)
                .map_err(|_| "REGISTRY_BAD_RESPONSE"),
            Err(PromiseError::Failed) => Err("REGISTRY_UNAVAILABLE"),
            Err(_) => Err("REGISTRY_BAD_RESPONSE"),
        };

        let reason: Option<String> = match &status {
            Err(code) => Some((*code).to_string()),
            Ok(_) if self.frozen => Some("FROZEN".into()),
            Ok(s) if s.blocked => Some(format!(
                "BLOCKLISTED:{}",
                s.blocked_by.clone().unwrap_or_else(|| "registry".into())
            )),
            Ok(s)
                if s.contract_code_hash.is_some()
                    && !flight.approved
                    && self.policy.contracts_require_allowlist
                    && !self.policy.allowlist.contains(&flight.receiver_id) =>
            {
                Some("CONTRACT_NOT_ALLOWLISTED".into())
            }
            Ok(_) => None,
        };

        if let Some(reason) = reason {
            self.rollback(&flight);
            return PromiseOrValue::Value(self.record_block(
                flight.id.0,
                now,
                &flight.receiver_id,
                flight.amount,
                &flight.purpose,
                &reason,
                &flight.signer_pk,
            ));
        }
        PromiseOrValue::Promise(self.transfer_with_callback(&flight))
    }

    #[private]
    pub fn on_transfer_done(&mut self, payment_id: U64) -> PayOutcome {
        let flight = self
            .in_flight
            .get(&payment_id.0)
            .cloned()
            .unwrap_or_else(|| env::panic_str("ERR_UNKNOWN_PAYMENT"));
        let now = env::block_timestamp();
        match env::promise_result_checked(0, 0) {
            Ok(_) | Err(PromiseError::TooLong(_)) => {
                self.in_flight.remove(&payment_id.0);
                self.in_flight_total = self.in_flight_total.saturating_sub(flight.amount);
                self.paid_total = add(self.paid_total, flight.amount);
                self.paid_count += 1;
                emit(
                    "payment_paid",
                    near_sdk::serde_json::json!({
                        "id": flight.id, "receiver_id": flight.receiver_id,
                        "amount": flight.amount, "purpose": flight.purpose,
                        "approved_by_owner": flight.approved, "signer_pk": flight.signer_pk,
                    }),
                );
                self.push_history(HistoryEntry {
                    id: flight.id,
                    ts: U64(now),
                    receiver_id: flight.receiver_id,
                    amount: flight.amount,
                    purpose: flight.purpose,
                    status: "paid".into(),
                    reason: if flight.approved { Some("OWNER_APPROVED".into()) } else { None },
                    signer_pk: flight.signer_pk,
                });
                PayOutcome { id: payment_id, status: "paid".into(), reason: None }
            }
            Err(_) => {
                // The transfer receipt failed (for example the receiver account does not
                // exist). NEAR refunds the deposit to this contract; undo the accounting.
                self.rollback(&flight);
                emit(
                    "payment_failed",
                    near_sdk::serde_json::json!({
                        "id": flight.id, "receiver_id": flight.receiver_id,
                        "amount": flight.amount, "reason": "TRANSFER_FAILED",
                    }),
                );
                self.push_history(HistoryEntry {
                    id: flight.id,
                    ts: U64(now),
                    receiver_id: flight.receiver_id,
                    amount: flight.amount,
                    purpose: flight.purpose,
                    status: "failed".into(),
                    reason: Some("TRANSFER_FAILED".into()),
                    signer_pk: flight.signer_pk,
                });
                PayOutcome {
                    id: payment_id,
                    status: "failed".into(),
                    reason: Some("TRANSFER_FAILED".into()),
                }
            }
        }
    }

    // ------------------------------------------------------------------
    // Views
    // ------------------------------------------------------------------

    pub fn get_policy(&self) -> Policy {
        self.policy.clone()
    }

    pub fn get_state(&self) -> StateView {
        StateView {
            owner_id: self.owner_id.clone(),
            frozen: self.frozen,
            frozen_reason: self.frozen_reason.clone(),
            balance: env::account_balance(),
            available: self.available(),
            pending_count: self.pending.len(),
            total_decisions: U64(self.history_total),
            paid_total: self.paid_total,
            paid_count: U64(self.paid_count),
            blocked_count: U64(self.blocked_count),
        }
    }

    pub fn get_spend(&self) -> SpendView {
        let now = env::block_timestamp();
        let live: Vec<&SpendEntry> =
            self.spends.iter().filter(|s| now.saturating_sub(s.ts.0) < DAY_NS).collect();
        let spent = live.iter().fold(NearToken::from_yoctonear(0), |acc, s| add(acc, s.amount));
        let mut per: Vec<(AccountId, NearToken)> = vec![];
        for s in &live {
            if let Some(e) = per.iter_mut().find(|(a, _)| *a == s.receiver_id) {
                e.1 = add(e.1, s.amount);
            } else {
                per.push((s.receiver_id.clone(), s.amount));
            }
        }
        SpendView {
            now: U64(now),
            window_ns: U64(DAY_NS),
            spent_24h: spent,
            daily_cap: self.policy.daily_cap,
            remaining_24h: self.policy.daily_cap.saturating_sub(spent),
            per_receiver: per,
            per_receiver_daily_cap: self.policy.per_receiver_daily_cap,
            attempts_last_hour: self
                .attempts
                .iter()
                .filter(|t| now.saturating_sub(**t) < HOUR_NS)
                .count() as u32,
            max_payments_per_hour: self.policy.max_payments_per_hour,
            blocked_last_hour: self
                .blocked_recent
                .iter()
                .filter(|t| now.saturating_sub(**t) < HOUR_NS)
                .count() as u32,
            in_flight: self.in_flight_total,
        }
    }

    pub fn get_pending(&self, from_index: Option<u32>, limit: Option<u32>) -> Vec<PendingRequest> {
        self.pending
            .values()
            .skip(from_index.unwrap_or(0) as usize)
            .take(limit.unwrap_or(MAX_PENDING).min(MAX_PENDING) as usize)
            .cloned()
            .collect()
    }

    /// Newest first. At most HISTORY_CAP entries are kept.
    pub fn get_history(&self, limit: Option<u32>) -> Vec<HistoryEntry> {
        let len = self.history.len() as u64;
        let limit = limit.unwrap_or(HISTORY_CAP).min(HISTORY_CAP) as u64;
        let mut out = vec![];
        let mut n = self.history_total;
        while n > 0 && (out.len() as u64) < limit.min(len) {
            n -= 1;
            let idx = (n % HISTORY_CAP as u64) as u32;
            if let Some(e) = self.history.get(idx) {
                out.push(e.clone());
            }
        }
        out
    }

    // ------------------------------------------------------------------
    // Internals
    // ------------------------------------------------------------------

    fn assert_owner(&self) {
        require!(env::predecessor_account_id() == self.owner_id, "ERR_NOT_OWNER");
    }

    /// The agent signs with a function-call key that lives on this contract account, so the
    /// predecessor is the contract itself.
    fn assert_agent(&self) {
        require!(
            env::predecessor_account_id() == env::current_account_id(),
            "ERR_NOT_AGENT: call with a key on the wallet account"
        );
    }

    fn signer_pk() -> String {
        String::from(&env::signer_account_pk())
    }

    fn take_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    fn validate_policy(p: &Policy) {
        require!(
            p.allowlist.len() <= MAX_LIST_LEN
                && p.denylist.len() <= MAX_LIST_LEN
                && p.known_contracts.len() <= MAX_LIST_LEN
                && p.guardians.len() <= MAX_LIST_LEN,
            "ERR_LIST_TOO_LONG"
        );
        require!(p.per_payment_cap <= p.daily_cap, "ERR_PER_PAYMENT_CAP_ABOVE_DAILY_CAP");
        require!(p.per_receiver_daily_cap <= p.daily_cap, "ERR_RECEIVER_CAP_ABOVE_DAILY_CAP");
        require!(p.min_purpose_len as usize <= MAX_PURPOSE_LEN, "ERR_MIN_PURPOSE_TOO_LONG");
    }

    fn prune(&mut self, now: u64) {
        self.spends.retain(|s| now.saturating_sub(s.ts.0) < DAY_NS);
        self.attempts.retain(|t| now.saturating_sub(*t) < HOUR_NS);
        self.blocked_recent.retain(|t| now.saturating_sub(*t) < HOUR_NS);
    }

    /// Rules that apply to every agent call, including requests for approval.
    fn hard_rules(
        &self,
        receiver_id: &AccountId,
        amount: NearToken,
        purpose: &str,
        _now: u64,
    ) -> Option<String> {
        if self.frozen {
            return Some("FROZEN".into());
        }
        if amount.as_yoctonear() == 0 {
            return Some("ZERO_AMOUNT".into());
        }
        if purpose.trim().len() < self.policy.min_purpose_len as usize {
            return Some("PURPOSE_REQUIRED".into());
        }
        if purpose.len() > MAX_PURPOSE_LEN {
            return Some("PURPOSE_TOO_LONG".into());
        }
        if *receiver_id == env::current_account_id() {
            return Some("SELF_PAYMENT".into());
        }
        if self.policy.denylist.contains(receiver_id) {
            return Some("DENYLISTED".into());
        }
        None
    }

    fn rate_rule(&self) -> Option<String> {
        if self.attempts.len() as u32 >= self.policy.max_payments_per_hour {
            return Some("RATE_LIMIT".into());
        }
        None
    }

    fn precheck(
        &self,
        receiver_id: &AccountId,
        amount: NearToken,
        purpose: &str,
        now: u64,
    ) -> Option<String> {
        if let Some(r) = self.hard_rules(receiver_id, amount, purpose, now) {
            return Some(r);
        }
        let allowlisted = self.policy.allowlist.contains(receiver_id);
        if self.policy.allowlist_mode && !allowlisted {
            return Some("NOT_ALLOWLISTED".into());
        }
        if self.policy.contracts_require_allowlist
            && self.policy.known_contracts.contains(receiver_id)
            && !allowlisted
        {
            return Some("CONTRACT_NOT_ALLOWLISTED".into());
        }
        self.rate_rule()
    }

    fn spent_24h(&self) -> NearToken {
        self.spends.iter().fold(NearToken::from_yoctonear(0), |acc, s| add(acc, s.amount))
    }

    fn spent_24h_to(&self, receiver_id: &AccountId) -> NearToken {
        self.spends
            .iter()
            .filter(|s| s.receiver_id == *receiver_id)
            .fold(NearToken::from_yoctonear(0), |acc, s| add(acc, s.amount))
    }

    fn cap_breach(&self, receiver_id: &AccountId, amount: NearToken) -> Option<String> {
        if amount > self.policy.per_payment_cap {
            return Some("OVER_PER_PAYMENT_CAP".into());
        }
        if add(self.spent_24h(), amount) > self.policy.daily_cap {
            return Some("OVER_DAILY_CAP".into());
        }
        if add(self.spent_24h_to(receiver_id), amount) > self.policy.per_receiver_daily_cap {
            return Some("OVER_RECEIVER_DAILY_CAP".into());
        }
        None
    }

    /// Balance that can leave the wallet: total minus storage staking, a storage reserve and
    /// payments already in flight.
    fn available(&self) -> NearToken {
        let storage = env::storage_byte_cost().saturating_mul(env::storage_usage() as u128);
        env::account_balance()
            .saturating_sub(storage)
            .saturating_sub(STORAGE_RESERVE)
            .saturating_sub(self.in_flight_total)
    }

    #[allow(clippy::too_many_arguments)]
    fn make_pending(
        &mut self,
        id: u64,
        now: u64,
        receiver_id: AccountId,
        amount: NearToken,
        purpose: String,
        reason: String,
        signer_pk: String,
    ) -> PromiseOrValue<PayOutcome> {
        if self.pending.len() >= MAX_PENDING {
            return PromiseOrValue::Value(self.record_block(
                id, now, &receiver_id, amount, &purpose, "PENDING_QUEUE_FULL", &signer_pk,
            ));
        }
        let req = PendingRequest {
            id: U64(id),
            created_at: U64(now),
            receiver_id,
            amount,
            purpose,
            reason: reason.clone(),
            signer_pk,
        };
        emit("payment_pending", near_sdk::serde_json::to_value(&req).unwrap());
        self.push_history(HistoryEntry {
            id: req.id,
            ts: U64(now),
            receiver_id: req.receiver_id.clone(),
            amount: req.amount,
            purpose: req.purpose.clone(),
            status: "pending".into(),
            reason: Some(reason.clone()),
            signer_pk: req.signer_pk.clone(),
        });
        self.pending.insert(id, req);
        PromiseOrValue::Value(PayOutcome { id: U64(id), status: "pending".into(), reason: Some(reason) })
    }

    #[allow(clippy::too_many_arguments)]
    fn record_block(
        &mut self,
        id: u64,
        now: u64,
        receiver_id: &AccountId,
        amount: NearToken,
        purpose: &str,
        reason: &str,
        signer_pk: &str,
    ) -> PayOutcome {
        self.blocked_count += 1;
        self.blocked_recent.push(now);
        emit(
            "payment_blocked",
            near_sdk::serde_json::json!({
                "id": U64(id), "receiver_id": receiver_id, "amount": amount,
                "purpose": purpose, "reason": reason, "signer_pk": signer_pk,
            }),
        );
        self.push_history(HistoryEntry {
            id: U64(id),
            ts: U64(now),
            receiver_id: receiver_id.clone(),
            amount,
            purpose: purpose.chars().take(MAX_PURPOSE_LEN).collect(),
            status: "blocked".into(),
            reason: Some(reason.to_string()),
            signer_pk: signer_pk.to_string(),
        });
        let n = self.policy.auto_freeze_after_blocks;
        if n > 0 && !self.frozen && self.blocked_recent.len() as u32 >= n {
            self.set_frozen("AUTO_FREEZE_TOO_MANY_BLOCKS".into(), env::current_account_id());
        }
        PayOutcome { id: U64(id), status: "blocked".into(), reason: Some(reason.to_string()) }
    }

    fn set_frozen(&mut self, reason: String, by: AccountId) {
        self.frozen = true;
        self.frozen_reason = Some(reason.clone());
        emit("frozen", near_sdk::serde_json::json!({ "by": by, "reason": reason }));
    }

    /// Reserve the spend, then check the shared blocklist (if configured) before moving NEAR.
    fn start_payment(&mut self, flight: InFlight) -> PromiseOrValue<PayOutcome> {
        let now = env::block_timestamp();
        self.spends.push(SpendEntry {
            id: flight.id,
            ts: U64(now),
            receiver_id: flight.receiver_id.clone(),
            amount: flight.amount,
        });
        self.in_flight_total = add(self.in_flight_total, flight.amount);
        self.in_flight.insert(flight.id.0, flight.clone());

        match self.policy.registry.clone() {
            Some(registry) => {
                let args = near_sdk::serde_json::json!({ "account_id": flight.receiver_id })
                    .to_string()
                    .into_bytes();
                let check = Promise::new(registry).function_call(
                    "get_status".to_string(),
                    args,
                    NearToken::from_yoctonear(0),
                    GAS_REGISTRY_CHECK,
                );
                let cb = Self::ext(env::current_account_id())
                    .with_static_gas(GAS_ON_REGISTRY)
                    .on_registry_checked(flight.id);
                PromiseOrValue::Promise(check.then(cb))
            }
            None => PromiseOrValue::Promise(self.transfer_with_callback(&flight)),
        }
    }

    fn transfer_with_callback(&self, flight: &InFlight) -> Promise {
        Promise::new(flight.receiver_id.clone()).transfer(flight.amount).then(
            Self::ext(env::current_account_id())
                .with_static_gas(GAS_ON_TRANSFER)
                .on_transfer_done(flight.id),
        )
    }

    /// Undo a reservation made by `start_payment`.
    fn rollback(&mut self, flight: &InFlight) {
        self.spends.retain(|s| s.id != flight.id);
        self.in_flight.remove(&flight.id.0);
        self.in_flight_total = self.in_flight_total.saturating_sub(flight.amount);
    }

    fn push_history(&mut self, entry: HistoryEntry) {
        let idx = (self.history_total % HISTORY_CAP as u64) as u32;
        if self.history.len() < HISTORY_CAP {
            self.history.push(entry);
        } else {
            self.history.set(idx, entry);
        }
        self.history_total += 1;
    }
}

#[cfg(test)]
mod tests;
