// Counterparty check: score a receiver before the agent pays it.
//
// Signals and where each one comes from (all free, no API key):
//   exists, balance, storage     NEAR RPC view_account
//   is a contract                NEAR RPC view_account.code_hash (all 1s means no code)
//   code changed since last seen local file demo/state/codehashes.json (first-seen pinning)
//   account age and activity     FastNEAR transactions API (tx.test.fastnear.com), with the
//                                NearBlocks testnet API as fallback (its testnet index lags)
//   prior interactions           the Leash wallet's own on-chain history (get_history)
//   shared blocklist             the Leash registry contract (get_status, get_reports)
//   owner policy                 the Leash wallet's policy (allowlist, denylist, contract rule)
//
// Returns { decision: "allow" | "warn" | "block", reasons: [...], signals: {...} }.
import fs from 'node:fs';
import path from 'node:path';
import { ROOT, view, viewAccount, fmtNear } from './near.mjs';

const STATE_DIR = path.join(ROOT, 'demo', 'state');
const CODE_HASH_FILE = path.join(STATE_DIR, 'codehashes.json');
const EMPTY_CODE = '11111111111111111111111111111111';
const HOUR_MS = 3600_000;
const RECEIVER_BLOCK_REASONS = /^(BLOCKLISTED|DENYLISTED|NOT_ALLOWLISTED|CONTRACT_NOT_ALLOWLISTED)/;

export const DEFAULTS = {
  newAccountHours: 72, // younger than this is "new"
  lowActivityTxs: 5, // fewer transactions than this is "low activity"
};

function loadJson(file, fallback) {
  try { return JSON.parse(fs.readFileSync(file, 'utf8')); } catch { return fallback; }
}
function saveJson(file, data) {
  fs.mkdirSync(path.dirname(file), { recursive: true });
  fs.writeFileSync(file, JSON.stringify(data, null, 2) + '\n');
}

/** Account age and activity. FastNEAR first, NearBlocks as fallback. */
export async function accountActivity(accountId) {
  try {
    const r = await fetch('https://tx.test.fastnear.com/v0/account', {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ account_id: accountId }),
      signal: AbortSignal.timeout(10_000),
    });
    if (!r.ok) throw new Error(`fastnear ${r.status}`);
    const j = await r.json();
    const txs = j.account_txs || [];
    const count = Number(j.txs_count ?? txs.length);
    const stamps = txs.map((t) => Number(BigInt(t.tx_block_timestamp) / 1_000_000n));
    const oldest = stamps.length ? Math.min(...stamps) : null;
    return {
      source: 'fastnear-tx-api',
      tx_count: count,
      first_seen_ms: oldest,
      // If there are more transactions than one page, the true first one is older still.
      age_is_lower_bound: count > txs.length,
    };
  } catch (e) {
    try {
      const r = await fetch(`https://api-testnet.nearblocks.io/v1/account/${accountId}`, {
        signal: AbortSignal.timeout(10_000),
      });
      const j = await r.json();
      const a = j.account?.[0];
      const created = a?.created?.block_timestamp ? Number(BigInt(a.created.block_timestamp) / 1_000_000n) : null;
      const c = await fetch(`https://api-testnet.nearblocks.io/v1/account/${accountId}/txns/count`, {
        signal: AbortSignal.timeout(10_000),
      }).then((x) => x.json()).catch(() => null);
      return {
        source: 'nearblocks-testnet',
        tx_count: Number(c?.txns?.[0]?.count ?? 0),
        first_seen_ms: created,
        age_is_lower_bound: false,
        note: 'NearBlocks testnet index can lag behind the chain',
      };
    } catch {
      return { source: 'unavailable', tx_count: null, first_seen_ms: null, error: String(e.message) };
    }
  }
}

/**
 * Check a receiver. `ctx` = { wallet, registry } contract ids.
 * Pure read: nothing is signed here.
 */
export async function checkCounterparty(receiverId, amountYocto, ctx, opts = {}) {
  const o = { ...DEFAULTS, ...opts };
  const reasons = [];
  const add = (level, code, detail) => reasons.push({ level, code, detail });
  const signals = { receiver_id: receiverId };

  const isImplicit = /^[0-9a-f]{64}$/.test(receiverId) || /^0x[0-9a-f]{40}$/.test(receiverId);
  const [acct, policy, registryStatus, reports, history] = await Promise.all([
    viewAccount(receiverId),
    view(ctx.wallet, 'get_policy').catch(() => null),
    ctx.registry ? view(ctx.registry, 'get_status', { account_id: receiverId }).catch(() => null) : null,
    ctx.registry ? view(ctx.registry, 'get_reports', { account_id: receiverId }).catch(() => []) : [],
    view(ctx.wallet, 'get_history', { limit: 100 }).catch(() => []),
  ]);

  // 1. Existence.
  signals.exists = !!acct;
  if (!acct) {
    if (isImplicit) add('warn', 'IMPLICIT_ACCOUNT_NOT_CREATED', 'paying will create this implicit account; nobody has used it yet');
    else add('block', 'NO_SUCH_ACCOUNT', 'named account does not exist, the transfer would fail');
  } else {
    signals.balance = fmtNear(acct.amount);
    signals.storage_bytes = acct.storage_usage;
  }

  // 2. Owner policy, read from chain.
  const allowlisted = !!policy?.allowlist?.includes(receiverId);
  signals.allowlisted_by_owner = allowlisted;
  if (policy?.denylist?.includes(receiverId)) add('block', 'WALLET_DENYLIST', 'the wallet owner denylisted this receiver');
  if (policy?.allowlist_mode && !allowlisted) add('block', 'NOT_ON_ALLOWLIST', 'wallet is in allowlist mode');
  if (policy && BigInt(amountYocto) > BigInt(policy.per_payment_cap)) {
    add('info', 'ABOVE_PER_PAYMENT_CAP', `over ${fmtNear(policy.per_payment_cap)} NEAR, the contract will queue it for the owner`);
  }

  // 3. Shared blocklist.
  if (registryStatus) {
    signals.registry = { blocked: registryStatus.blocked, blocked_by: registryStatus.blocked_by, reports: registryStatus.reports, threshold: registryStatus.threshold };
    signals.registry_evidence = (reports || []).map((r) => ({ reporter: r.reporter, reason: r.reason, evidence_url: r.evidence_url }));
    if (registryStatus.blocked) {
      add('block', 'SHARED_BLOCKLIST', `blocked by ${registryStatus.blocked_by} (${registryStatus.reports}/${registryStatus.threshold} reports)`);
    } else if (registryStatus.reports > 0) {
      add('warn', 'REPORTED', `${registryStatus.reports} report(s), below the threshold of ${registryStatus.threshold}`);
    }
  } else if (ctx.registry) {
    add('warn', 'REGISTRY_UNREACHABLE', 'could not read the shared blocklist');
  }

  // 4. Contract or not, and code pinning.
  if (acct) {
    const isContract = acct.code_hash !== EMPTY_CODE;
    signals.is_contract = isContract;
    signals.code_hash = isContract ? acct.code_hash : null;
    const pins = loadJson(CODE_HASH_FILE, {});
    const pinned = pins[receiverId];
    if (isContract) {
      if (!pinned) {
        pins[receiverId] = { code_hash: acct.code_hash, first_seen: new Date().toISOString() };
        if (!o.dryRun) saveJson(CODE_HASH_FILE, pins);
        signals.code_first_seen = 'now';
      } else if (pinned.code_hash !== acct.code_hash) {
        add('warn', 'CODE_CHANGED', `code hash changed from ${pinned.code_hash} (first seen ${pinned.first_seen}) to ${acct.code_hash}`);
        signals.code_previous = pinned.code_hash;
      } else {
        signals.code_first_seen = pinned.first_seen;
      }
      if (!allowlisted) {
        if (policy?.contracts_require_allowlist) add('block', 'CONTRACT_NOT_ALLOWLISTED', 'receiver is a contract and the owner requires contracts to be allowlisted');
        else add('warn', 'CONTRACT_RECEIVER', 'receiver is a contract; NEAR sent to it may be hard to recover');
      }
    } else if (pinned && pinned.code_hash !== EMPTY_CODE) {
      add('warn', 'CODE_REMOVED', `was a contract (${pinned.code_hash}), now has no code`);
    }
  }

  // 5. Prior interactions with this agent, from the wallet's on-chain history.
  const mine = (history || []).filter((h) => h.receiver_id === receiverId);
  const paid = mine.filter((h) => h.status === 'paid');
  signals.prior = {
    paid_count: paid.length,
    paid_total: fmtNear(paid.reduce((a, h) => a + BigInt(h.amount), 0n)),
    // Only count blocks that were about this receiver, not about the wallet's own state
    // (FROZEN, RATE_LIMIT, PURPOSE_REQUIRED and so on).
    blocked_count: mine.filter((h) => h.status === 'blocked' && RECEIVER_BLOCK_REASONS.test(h.reason || '')).length,
    last_paid: paid[0] ? new Date(Number(BigInt(paid[0].ts) / 1_000_000n)).toISOString() : null,
  };
  if (signals.prior.blocked_count > 0) {
    add('warn', 'PREVIOUSLY_BLOCKED', `the wallet blocked ${signals.prior.blocked_count} earlier payment(s) to this receiver`);
  }

  // 6. Age and activity. Only matters when there is no trust yet.
  if (acct) {
    const act = await accountActivity(receiverId);
    signals.activity = act;
    const trusted = allowlisted || paid.length > 0;
    if (act.first_seen_ms) {
      const ageH = (Date.now() - act.first_seen_ms) / HOUR_MS;
      signals.age_hours = Math.round(ageH * 10) / 10;
      if (ageH < o.newAccountHours && !trusted) add('warn', 'NEW_ACCOUNT', `first seen ${signals.age_hours}h ago, no prior payments, not allowlisted`);
    } else if (act.source === 'unavailable' && !trusted) {
      add('warn', 'AGE_UNKNOWN', 'no indexer reachable, account age unknown');
    }
    if (act.tx_count !== null && act.tx_count < o.lowActivityTxs && !trusted) {
      add('warn', 'LOW_ACTIVITY', `${act.tx_count} transaction(s) on record`);
    }
    if (trusted) add('info', 'TRUSTED', allowlisted ? 'allowlisted by the wallet owner' : `paid ${paid.length} time(s) before`);
  }

  const decision = reasons.some((r) => r.level === 'block') ? 'block'
    : reasons.some((r) => r.level === 'warn') ? 'warn' : 'allow';
  return { decision, receiver_id: receiverId, amount: amountYocto, reasons, signals, checked_at: new Date().toISOString() };
}

export function formatCheck(c) {
  const icon = { allow: 'ALLOW', warn: 'WARN ', block: 'BLOCK' }[c.decision];
  const lines = [`[counterparty] ${icon}  ${c.receiver_id}  ${fmtNear(c.amount)} NEAR`];
  for (const r of c.reasons) lines.push(`   ${r.level.padEnd(5)} ${r.code}: ${r.detail}`);
  return lines.join('\n');
}
