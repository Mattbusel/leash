//! Leash registry: a shared blocklist that agents and owners report bad receivers to.
//!
//! A receiver counts as blocked once `threshold` distinct reporters have filed a report
//! with an evidence URL, or once the registry owner blocks it directly. Wallets call
//! `get_status` as a cross-contract call before every transfer.
//!
//! Attesters (trusted off-chain checkers) can also record the code hash of accounts they
//! have seen deployed as contracts, because a NEAR contract cannot read another account's
//! code on chain.

use near_sdk::json_types::U64;
use near_sdk::store::{IterableMap, IterableSet, LookupMap};
use near_sdk::{env, near, require, AccountId, BorshStorageKey, NearToken, PanicOnDefault};

pub const MAX_REASON_LEN: usize = 200;
pub const MAX_URL_LEN: usize = 300;
pub const MAX_REPORTS_PER_TARGET: usize = 50;

#[derive(BorshStorageKey)]
#[near]
enum StorageKey {
    Reports,
    Targets,
    OwnerBlocked,
    Attesters,
    CodeHashes,
}

#[near(serializers = [borsh, json])]
#[derive(Clone, Debug)]
pub struct Report {
    pub reporter: AccountId,
    pub reason: String,
    pub evidence_url: String,
    pub ts: U64,
}

#[near(serializers = [borsh, json])]
#[derive(Clone, Debug)]
pub struct CodeAttestation {
    pub code_hash: String,
    pub attester: AccountId,
    pub ts: U64,
}

#[near(serializers = [json])]
#[derive(Clone, Debug, PartialEq)]
pub struct Status {
    pub account_id: AccountId,
    pub blocked: bool,
    /// "owner" or "reports" when blocked.
    pub blocked_by: Option<String>,
    pub reports: u32,
    pub threshold: u32,
    pub owner_reason: Option<String>,
    pub contract_code_hash: Option<String>,
}

#[near(serializers = [json])]
pub struct Config {
    pub owner_id: AccountId,
    pub threshold: u32,
    pub report_deposit: NearToken,
    pub attesters: Vec<AccountId>,
    pub targets: u32,
}

#[near(contract_state)]
#[derive(PanicOnDefault)]
pub struct Registry {
    owner_id: AccountId,
    threshold: u32,
    report_deposit: NearToken,
    reports: LookupMap<AccountId, Vec<Report>>,
    targets: IterableSet<AccountId>,
    owner_blocked: IterableMap<AccountId, String>,
    attesters: IterableSet<AccountId>,
    code_hashes: LookupMap<AccountId, CodeAttestation>,
}

fn emit(event: &str, data: near_sdk::serde_json::Value) {
    let ev = near_sdk::serde_json::json!({
        "standard": "leash-registry",
        "version": "1.0.0",
        "event": event,
        "data": [data],
    });
    env::log_str(&format!("EVENT_JSON:{}", ev));
}

#[near]
impl Registry {
    #[init]
    pub fn new(owner_id: AccountId, threshold: u32, report_deposit: Option<NearToken>) -> Self {
        require!(threshold >= 1, "ERR_THRESHOLD_ZERO");
        Self {
            owner_id,
            threshold,
            report_deposit: report_deposit.unwrap_or(NearToken::from_millinear(10)),
            reports: LookupMap::new(StorageKey::Reports),
            targets: IterableSet::new(StorageKey::Targets),
            owner_blocked: IterableMap::new(StorageKey::OwnerBlocked),
            attesters: IterableSet::new(StorageKey::Attesters),
            code_hashes: LookupMap::new(StorageKey::CodeHashes),
        }
    }

    /// File a report. One report per reporter per account. The deposit pays for storage and
    /// makes spam cost something.
    #[payable]
    pub fn report(&mut self, account_id: AccountId, reason: String, evidence_url: String) -> Status {
        require!(
            env::attached_deposit() >= self.report_deposit,
            format!("ERR_DEPOSIT: attach at least {}", self.report_deposit.exact_amount_display())
        );
        let reporter = env::predecessor_account_id();
        require!(reporter != account_id, "ERR_SELF_REPORT");
        require!(!reason.trim().is_empty() && reason.len() <= MAX_REASON_LEN, "ERR_REASON");
        require!(
            (evidence_url.starts_with("https://") || evidence_url.starts_with("http://"))
                && evidence_url.len() <= MAX_URL_LEN,
            "ERR_EVIDENCE_URL: must be an http(s) URL"
        );
        let mut list = self.reports.get(&account_id).cloned().unwrap_or_default();
        require!(!list.iter().any(|r| r.reporter == reporter), "ERR_ALREADY_REPORTED");
        require!(list.len() < MAX_REPORTS_PER_TARGET, "ERR_TOO_MANY_REPORTS");
        let was_blocked = self.is_blocked_by_reports(list.len());
        list.push(Report {
            reporter: reporter.clone(),
            reason: reason.clone(),
            evidence_url: evidence_url.clone(),
            ts: U64(env::block_timestamp()),
        });
        let count = list.len();
        self.reports.insert(account_id.clone(), list);
        self.targets.insert(account_id.clone());
        emit(
            "report_filed",
            near_sdk::serde_json::json!({
                "account_id": account_id, "reporter": reporter, "reason": reason,
                "evidence_url": evidence_url, "reports": count,
            }),
        );
        if !was_blocked && self.is_blocked_by_reports(count) {
            emit(
                "account_blocked",
                near_sdk::serde_json::json!({ "account_id": account_id, "by": "reports", "reports": count }),
            );
        }
        self.get_status(account_id)
    }

    /// Withdraw your own report.
    pub fn retract(&mut self, account_id: AccountId) {
        let reporter = env::predecessor_account_id();
        let mut list = self
            .reports
            .get(&account_id)
            .cloned()
            .unwrap_or_else(|| env::panic_str("ERR_NO_REPORTS"));
        let before = list.len();
        list.retain(|r| r.reporter != reporter);
        require!(list.len() < before, "ERR_NOT_YOUR_REPORT");
        self.reports.insert(account_id.clone(), list);
        emit(
            "report_retracted",
            near_sdk::serde_json::json!({ "account_id": account_id, "reporter": reporter }),
        );
    }

    // ---------------- owner ----------------

    pub fn block(&mut self, account_id: AccountId, reason: String) {
        self.assert_owner();
        require!(reason.len() <= MAX_REASON_LEN, "ERR_REASON");
        self.owner_blocked.insert(account_id.clone(), reason.clone());
        self.targets.insert(account_id.clone());
        emit(
            "account_blocked",
            near_sdk::serde_json::json!({ "account_id": account_id, "by": "owner", "reason": reason }),
        );
    }

    /// Clear an owner block and all reports for an account (for false positives).
    pub fn unblock(&mut self, account_id: AccountId) {
        self.assert_owner();
        self.owner_blocked.remove(&account_id);
        self.reports.remove(&account_id);
        emit("account_unblocked", near_sdk::serde_json::json!({ "account_id": account_id }));
    }

    pub fn set_threshold(&mut self, threshold: u32) {
        self.assert_owner();
        require!(threshold >= 1, "ERR_THRESHOLD_ZERO");
        self.threshold = threshold;
        emit("threshold_set", near_sdk::serde_json::json!({ "threshold": threshold }));
    }

    pub fn set_report_deposit(&mut self, deposit: NearToken) {
        self.assert_owner();
        self.report_deposit = deposit;
    }

    pub fn add_attester(&mut self, account_id: AccountId) {
        self.assert_owner();
        self.attesters.insert(account_id.clone());
        emit("attester_added", near_sdk::serde_json::json!({ "account_id": account_id }));
    }

    pub fn remove_attester(&mut self, account_id: AccountId) {
        self.assert_owner();
        self.attesters.remove(&account_id);
        emit("attester_removed", near_sdk::serde_json::json!({ "account_id": account_id }));
    }

    /// Record that `account_id` has contract code with `code_hash` (or clear it with None).
    pub fn attest_code(&mut self, account_id: AccountId, code_hash: Option<String>) {
        let caller = env::predecessor_account_id();
        require!(caller == self.owner_id || self.attesters.contains(&caller), "ERR_NOT_ATTESTER");
        match code_hash {
            Some(h) => {
                require!(h.len() <= 64, "ERR_CODE_HASH");
                emit(
                    "code_attested",
                    near_sdk::serde_json::json!({ "account_id": account_id, "code_hash": h, "attester": caller }),
                );
                self.code_hashes.insert(
                    account_id,
                    CodeAttestation { code_hash: h, attester: caller, ts: U64(env::block_timestamp()) },
                );
            }
            None => {
                self.code_hashes.remove(&account_id);
            }
        }
    }

    // ---------------- views ----------------

    pub fn get_status(&self, account_id: AccountId) -> Status {
        let reports = self.reports.get(&account_id).map(|l| l.len()).unwrap_or(0);
        let owner_reason = self.owner_blocked.get(&account_id).cloned();
        let blocked_by = if owner_reason.is_some() {
            Some("owner".to_string())
        } else if self.is_blocked_by_reports(reports) {
            Some("reports".to_string())
        } else {
            None
        };
        Status {
            account_id: account_id.clone(),
            blocked: blocked_by.is_some(),
            blocked_by,
            reports: reports as u32,
            threshold: self.threshold,
            owner_reason,
            contract_code_hash: self.code_hashes.get(&account_id).map(|a| a.code_hash.clone()),
        }
    }

    pub fn get_reports(&self, account_id: AccountId) -> Vec<Report> {
        self.reports.get(&account_id).cloned().unwrap_or_default()
    }

    pub fn get_attestation(&self, account_id: AccountId) -> Option<CodeAttestation> {
        self.code_hashes.get(&account_id).cloned()
    }

    /// Every reported or owner-blocked account with its status.
    pub fn list_targets(&self, from_index: Option<u32>, limit: Option<u32>) -> Vec<Status> {
        self.targets
            .iter()
            .skip(from_index.unwrap_or(0) as usize)
            .take(limit.unwrap_or(50).min(100) as usize)
            .map(|a| self.get_status(a.clone()))
            .collect()
    }

    pub fn get_config(&self) -> Config {
        Config {
            owner_id: self.owner_id.clone(),
            threshold: self.threshold,
            report_deposit: self.report_deposit,
            attesters: self.attesters.iter().cloned().collect(),
            targets: self.targets.len(),
        }
    }

    fn is_blocked_by_reports(&self, reports: usize) -> bool {
        reports as u32 >= self.threshold
    }

    fn assert_owner(&self) {
        require!(env::predecessor_account_id() == self.owner_id, "ERR_NOT_OWNER");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use near_sdk::test_utils::{get_logs, VMContextBuilder};
    use near_sdk::testing_env;

    fn acc(s: &str) -> AccountId {
        s.parse().unwrap()
    }

    fn ctx(pred: &str, deposit: NearToken) {
        testing_env!(VMContextBuilder::new()
            .current_account_id(acc("registry.testnet"))
            .predecessor_account_id(acc(pred))
            .attached_deposit(deposit)
            .block_timestamp(42)
            .build());
    }

    fn fee() -> NearToken {
        NearToken::from_millinear(10)
    }

    fn reg(threshold: u32) -> Registry {
        ctx("owner.testnet", NearToken::from_yoctonear(0));
        Registry::new(acc("owner.testnet"), threshold, None)
    }

    const URL: &str = "https://example.com/evidence/drainer";

    #[test]
    fn blocks_after_threshold_distinct_reporters() {
        let mut r = reg(2);
        ctx("alice.testnet", fee());
        let s = r.report(acc("drainer.testnet"), "phishing invoice".into(), URL.into());
        assert!(!s.blocked);
        assert_eq!(s.reports, 1);
        ctx("bob.testnet", fee());
        let s = r.report(acc("drainer.testnet"), "drained my agent".into(), URL.into());
        assert!(s.blocked);
        assert_eq!(s.blocked_by.as_deref(), Some("reports"));
        assert!(get_logs().iter().any(|l| l.contains("\"event\":\"account_blocked\"")));
        assert_eq!(r.get_reports(acc("drainer.testnet")).len(), 2);
    }

    #[test]
    #[should_panic(expected = "ERR_ALREADY_REPORTED")]
    fn same_reporter_counts_once() {
        let mut r = reg(2);
        ctx("alice.testnet", fee());
        r.report(acc("drainer.testnet"), "phishing".into(), URL.into());
        r.report(acc("drainer.testnet"), "phishing again".into(), URL.into());
    }

    #[test]
    #[should_panic(expected = "ERR_DEPOSIT")]
    fn report_needs_deposit() {
        let mut r = reg(2);
        ctx("alice.testnet", NearToken::from_yoctonear(1));
        r.report(acc("drainer.testnet"), "phishing".into(), URL.into());
    }

    #[test]
    #[should_panic(expected = "ERR_EVIDENCE_URL")]
    fn report_needs_evidence_url() {
        let mut r = reg(2);
        ctx("alice.testnet", fee());
        r.report(acc("drainer.testnet"), "phishing".into(), "trust me".into());
    }

    #[test]
    #[should_panic(expected = "ERR_SELF_REPORT")]
    fn cannot_report_self() {
        let mut r = reg(2);
        ctx("alice.testnet", fee());
        r.report(acc("alice.testnet"), "phishing".into(), URL.into());
    }

    #[test]
    fn owner_block_and_unblock() {
        let mut r = reg(3);
        ctx("owner.testnet", NearToken::from_yoctonear(0));
        r.block(acc("drainer.testnet"), "known drainer".into());
        let s = r.get_status(acc("drainer.testnet"));
        assert!(s.blocked);
        assert_eq!(s.blocked_by.as_deref(), Some("owner"));
        assert_eq!(r.list_targets(None, None).len(), 1);
        r.unblock(acc("drainer.testnet"));
        assert!(!r.get_status(acc("drainer.testnet")).blocked);
    }

    #[test]
    #[should_panic(expected = "ERR_NOT_OWNER")]
    fn only_owner_blocks() {
        let mut r = reg(3);
        ctx("alice.testnet", NearToken::from_yoctonear(0));
        r.block(acc("vendor.testnet"), "griefing".into());
    }

    #[test]
    fn retract_drops_below_threshold() {
        let mut r = reg(2);
        ctx("alice.testnet", fee());
        r.report(acc("drainer.testnet"), "phishing".into(), URL.into());
        ctx("bob.testnet", fee());
        r.report(acc("drainer.testnet"), "phishing".into(), URL.into());
        assert!(r.get_status(acc("drainer.testnet")).blocked);
        r.retract(acc("drainer.testnet"));
        assert!(!r.get_status(acc("drainer.testnet")).blocked);
    }

    #[test]
    fn threshold_change_applies_immediately() {
        let mut r = reg(3);
        ctx("alice.testnet", fee());
        r.report(acc("drainer.testnet"), "phishing".into(), URL.into());
        assert!(!r.get_status(acc("drainer.testnet")).blocked);
        ctx("owner.testnet", NearToken::from_yoctonear(0));
        r.set_threshold(1);
        assert!(r.get_status(acc("drainer.testnet")).blocked);
    }

    #[test]
    fn attesters_record_code_hashes() {
        let mut r = reg(2);
        ctx("owner.testnet", NearToken::from_yoctonear(0));
        r.add_attester(acc("checker.testnet"));
        ctx("checker.testnet", NearToken::from_yoctonear(0));
        r.attest_code(acc("dex.testnet"), Some("8tY2kNq3".into()));
        assert_eq!(r.get_status(acc("dex.testnet")).contract_code_hash.as_deref(), Some("8tY2kNq3"));
        assert_eq!(r.get_config().attesters, vec![acc("checker.testnet")]);
    }

    #[test]
    #[should_panic(expected = "ERR_NOT_ATTESTER")]
    fn strangers_cannot_attest() {
        let mut r = reg(2);
        ctx("alice.testnet", NearToken::from_yoctonear(0));
        r.attest_code(acc("dex.testnet"), Some("x".into()));
    }

    #[test]
    fn unknown_account_is_clean() {
        let r = reg(2);
        let s = r.get_status(acc("nobody.testnet"));
        assert!(!s.blocked && s.reports == 0 && s.contract_code_hash.is_none());
    }
}
