// End-to-end test against the live testnet deployment. Every step sends real
// transactions and checks the result. Re-runnable: caps are set relative to what was
// already spent in the last 24 hours, and the vendor returns what it received.
//
//   node scripts/e2e.mjs
import fs from 'node:fs';
import path from 'node:path';
import {
  ROOT, readEnv, readDeployments, accountWithKey, view, call, send, actions, near, fmtNear,
  viewAccount, viewAccessKeys, KeyPair, TGAS, EXPLORER_TX, describeError,
} from '../agent/near.mjs';
import { guardedPay } from '../agent/guard.mjs';
import { checkCounterparty } from '../agent/counterparty.mjs';
import { relativeBaseline } from '../agent/policy.mjs';

const d = readDeployments();
const env = readEnv();
const owner = accountWithKey(d.owner, env.OWNER_KEY);
const agent = accountWithKey(d.wallet, env.AGENT_KEY);
const commons = accountWithKey(d.registry_owner, env.COMMONS_KEY);
const scout1 = accountWithKey(d.scouts[0], env.SCOUT1_KEY);
const scout2 = accountWithKey(d.scouts[1], env.SCOUT2_KEY);
const vendor = accountWithKey(d.vendor, env.VENDOR_KEY);
const GHOST = `ghost-${Date.now().toString(36)}.${d.owner}`;

const results = [];
let failed = 0;

function record(name, pass, info = {}) {
  results.push({ name, pass, ...info });
  if (!pass) failed++;
  const link = info.tx ? `  ${EXPLORER_TX}${info.tx}` : '';
  console.log(`${pass ? 'PASS' : 'FAIL'}  ${name}${info.detail ? '  [' + info.detail + ']' : ''}${link}`);
}

const pay = (receiver_id, amount, purpose = 'e2e invoice 0001 for API credits', signer = agent) =>
  call(signer, d.wallet, 'pay', { receiver_id, amount: near(amount), purpose });
const asOwner = (method, args, gasTgas = 100) => call(owner, d.wallet, method, args, { gasTgas });

async function setPolicy(patch) {
  const p = { ...(await view(d.wallet, 'get_policy')), ...patch };
  const r = await asOwner('set_policy', { policy: p });
  if (!r.ok) throw new Error('set_policy failed: ' + JSON.stringify(r.failure));
  return r;
}

function outcome(r) { return r.value || {}; }

function expectOutcome(name, r, status, reason) {
  const o = outcome(r);
  const pass = r.ok && o.status === status && (reason === undefined || o.reason === reason);
  record(name, pass, { tx: r.hash, detail: `${o.status}${o.reason ? ' ' + o.reason : ''}` });
  return o;
}

function expectTxFailure(name, r, needle) {
  const msg = JSON.stringify(r.failure || '');
  record(name, !r.ok && msg.includes(needle), { tx: r.hash, detail: (msg.match(/ERR_[A-Z_]+|Method \w+ is private/) || [msg.slice(0, 80)])[0] });
}

/** A transaction the protocol must refuse before it ever executes. */
async function expectRejectedByProtocol(name, actionList, receiverId, needle) {
  try {
    const r = await send(agent, receiverId, actionList);
    record(name, false, { tx: r.hash, detail: 'transaction was accepted' });
  } catch (e) {
    const msg = describeError(e);
    record(name, msg.includes(needle), { detail: msg.replace(/\s+/g, ' ').slice(0, 160) });
  }
}

async function main() {
  console.log(`Leash e2e on testnet: wallet ${d.wallet}, registry ${d.registry}\n`);
  const t0 = Date.now();

  // ---------------------------------------------------------------- 0. preflight
  const wAcc = await viewAccount(d.wallet);
  record('wallet contract is deployed', wAcc && wAcc.code_hash !== '11111111111111111111111111111111', { detail: wAcc?.code_hash });
  const agentPub = KeyPair.fromString(env.AGENT_KEY).getPublicKey().toString();
  const keys = await viewAccessKeys(d.wallet);
  const perm = keys.find((k) => k.public_key === agentPub)?.access_key.permission;
  record('agent key is a function-call key scoped to pay/request_payment on the wallet',
    !!perm?.FunctionCall && perm.FunctionCall.receiver_id === d.wallet &&
    JSON.stringify(perm.FunctionCall.method_names) === '["pay","request_payment"]' && perm.FunctionCall.allowance !== null,
    { detail: perm ? `allowance ${fmtNear(perm.FunctionCall?.allowance)} NEAR` : 'missing' });

  // Top up the wallet from the owner if a lot of runs have drained it.
  let state = await view(d.wallet, 'get_state');
  if (BigInt(state.available) < BigInt(near('0.6'))) {
    const r = await send(owner, d.wallet, [actions.transfer(BigInt(near('1')))]);
    console.log(`(topped up wallet with 1 NEAR from owner ${r.url})`);
  }

  // ---------------------------------------------------------------- 1. owner baseline
  if (state.frozen) await asOwner('unfreeze', {});
  for (const p of await view(d.wallet, 'get_pending', {})) await asOwner('reject', { request_id: p.id, note: 'e2e cleanup' });
  const baseline = await relativeBaseline(d);
  let r = await asOwner('set_policy', { policy: baseline });
  record('owner sets policy (caps relative to the last 24h)', r.ok && r.events.some((e) => e.event === 'policy_updated'), { tx: r.hash });

  // ---------------------------------------------------------------- 2. shared blocklist
  for (const [acc, reason] of [[scout1, 'fake vendor invoice asking agents to pay this account'], [scout2, 'prompt injection in a PDF invoice pointed here']]) {
    const reports = await view(d.registry, 'get_reports', { account_id: d.drainer });
    if (reports.some((x) => x.reporter === acc.accountId)) continue;
    const cfg = await view(d.registry, 'get_config');
    r = await call(acc, d.registry, 'report', {
      account_id: d.drainer, reason, evidence_url: 'https://github.com/Mattbusel/leash/blob/main/demo/invoice-injected.md',
    }, { deposit: cfg.report_deposit });
    record(`${acc.accountId} reports the drainer with evidence`, r.ok, { tx: r.hash, detail: `reports ${r.value?.reports}` });
  }
  const st = await view(d.registry, 'get_status', { account_id: d.drainer });
  record('drainer is blocked once 2 distinct reporters agree', st.blocked && st.blocked_by === 'reports', { detail: `${st.reports}/${st.threshold}` });
  const regAcc = await viewAccount(d.registry);
  r = await call(commons, d.registry, 'attest_code', { account_id: d.registry, code_hash: regAcc.code_hash });
  record('registry operator attests a contract code hash', r.ok, { tx: r.hash });
  r = await call(scout1, d.registry, 'report', { account_id: d.vendor, reason: 'no deposit', evidence_url: 'https://example.com' }, { deposit: '0' });
  expectTxFailure('report without the anti-spam deposit is refused', r, 'ERR_DEPOSIT');

  // ---------------------------------------------------------------- 3. legit payment through the guard
  const g = await guardedPay({ receiver_id: d.vendor, amount: near('0.01'), purpose: 'e2e invoice 0001 for API credits' }, { mode: 'deny', log: () => {} });
  record('guard allows the allowlisted vendor and the payment settles (registry check + transfer + callback)',
    g.check.decision === 'allow' && g.tx?.value?.status === 'paid', { tx: g.tx?.hash, detail: g.tx?.value?.status });

  // ---------------------------------------------------------------- 4. over the per-payment cap -> pending -> approve
  r = await pay(d.vendor, '0.15', 'e2e annual plan, above the per-payment cap');
  const pend = expectOutcome('payment above the per-payment cap becomes a pending request', r, 'pending', 'OVER_PER_PAYMENT_CAP');
  r = await asOwner('approve', { request_id: pend.id }, 150);
  record('owner approves it and it settles', r.ok && r.value?.status === 'paid' && r.events.some((e) => e.event === 'request_approved'), { tx: r.hash, detail: r.value?.status });

  // ---------------------------------------------------------------- 5. request_payment -> reject
  r = await call(agent, d.wallet, 'request_payment', { receiver_id: d.vendor, amount: near('0.02'), purpose: 'e2e agent asks before paying' });
  const req = expectOutcome('agent request_payment goes to the owner queue', r, 'pending', 'AGENT_REQUEST');
  r = await asOwner('reject', { request_id: req.id, note: 'not needed' });
  record('owner rejects it', r.ok && r.events.some((e) => e.event === 'request_rejected'), { tx: r.hash });

  // ---------------------------------------------------------------- 6. shared blocklist enforced on chain
  r = await pay(d.drainer, '0.01', 'invoice 7781 settlement remit now');
  expectOutcome('contract refuses the drainer via the cross-contract registry check', r, 'blocked', 'BLOCKLISTED:reports');

  // ---------------------------------------------------------------- 7. contract receiver rule
  r = await pay(d.registry, '0.01', 'e2e pay a contract receiver');
  expectOutcome('contract receiver (attested in the registry) needs the allowlist', r, 'blocked', 'CONTRACT_NOT_ALLOWLISTED');

  // ---------------------------------------------------------------- 8. denylist
  await setPolicy({ denylist: [d.drainer] });
  r = await pay(d.drainer, '0.01', 'invoice 7781 settlement remit now');
  expectOutcome('denylist blocks without needing the registry', r, 'blocked', 'DENYLISTED');
  await setPolicy({ denylist: [] });

  // ---------------------------------------------------------------- 9. memo rule
  r = await pay(d.vendor, '0.01', 'pay');
  expectOutcome('a purpose string is required', r, 'blocked', 'PURPOSE_REQUIRED');

  // ---------------------------------------------------------------- 10. rolling caps
  let sp = await view(d.wallet, 'get_spend');
  const vNow = BigInt((sp.per_receiver.find(([a]) => a === d.vendor) || [0, '0'])[1]);
  await setPolicy({ per_receiver_daily_cap: (vNow + BigInt(near('0.02'))).toString() });
  r = await pay(d.vendor, '0.05', 'e2e would exceed the per-receiver cap');
  const pr = expectOutcome('per-receiver rolling 24h cap queues the payment', r, 'pending', 'OVER_RECEIVER_DAILY_CAP');
  await asOwner('reject', { request_id: pr.id, note: 'e2e' });
  sp = await view(d.wallet, 'get_spend');
  await setPolicy({ per_receiver_daily_cap: sp.spent_24h, daily_cap: (BigInt(sp.spent_24h) + BigInt(near('0.03'))).toString() });
  r = await pay(d.scouts[1], '0.05', 'e2e would exceed the daily cap');
  const dc = expectOutcome('global rolling 24h cap queues the payment', r, 'pending', 'OVER_DAILY_CAP');
  await asOwner('reject', { request_id: dc.id, note: 'e2e' });
  await setPolicy({ daily_cap: baseline.daily_cap, per_receiver_daily_cap: baseline.per_receiver_daily_cap });

  // ---------------------------------------------------------------- 11. allowlist mode
  await setPolicy({ allowlist_mode: true });
  r = await pay(d.scouts[1], '0.01', 'e2e pay someone not on the allowlist');
  expectOutcome('allowlist mode blocks unknown receivers', r, 'blocked', 'NOT_ALLOWLISTED');
  await setPolicy({ allowlist_mode: false });

  // ---------------------------------------------------------------- 12. failed transfer rolls back
  sp = await view(d.wallet, 'get_spend');
  r = await pay(GHOST, '0.01', 'e2e pay an account that does not exist');
  const sp2 = await view(d.wallet, 'get_spend');
  record('transfer to a missing account fails and the reservation is rolled back',
    r.ok && r.value?.status === 'failed' && r.value?.reason === 'TRANSFER_FAILED' && sp2.spent_24h === sp.spent_24h && sp2.in_flight === '0',
    { tx: r.hash, detail: `${r.value?.status} ${r.value?.reason}, spent unchanged ${fmtNear(sp2.spent_24h)}` });

  // ---------------------------------------------------------------- 13. rate limit
  sp = await view(d.wallet, 'get_spend');
  await setPolicy({ max_payments_per_hour: sp.attempts_last_hour + 1 });
  await pay(d.vendor, '0.001', 'e2e rate limit filler call');
  r = await pay(d.vendor, '0.001', 'e2e rate limit second call');
  expectOutcome('max payments per hour throttles the agent', r, 'blocked', 'RATE_LIMIT');
  await setPolicy({ max_payments_per_hour: 200 });

  // ---------------------------------------------------------------- 14. freeze / guardian
  r = await call(scout1, d.wallet, 'freeze', { reason: 'e2e guardian kill switch' });
  record('guardian can freeze the wallet', r.ok && r.events.some((e) => e.event === 'frozen'), { tx: r.hash });
  r = await pay(d.vendor, '0.01');
  expectOutcome('frozen wallet blocks every payment', r, 'blocked', 'FROZEN');
  r = await call(scout1, d.wallet, 'unfreeze', {});
  expectTxFailure('guardian cannot unfreeze', r, 'ERR_NOT_OWNER');
  r = await asOwner('unfreeze', {});
  record('owner unfreezes', r.ok, { tx: r.hash });

  // ---------------------------------------------------------------- 15. auto-freeze tripwire
  await setPolicy({ auto_freeze_after_blocks: 2, denylist: [d.drainer] });
  await pay(d.drainer, '0.01', 'invoice 7781 settlement remit now');
  r = await pay(d.drainer, '0.01', 'invoice 7781 settlement remit now');
  state = await view(d.wallet, 'get_state');
  record('two blocked attempts trip the auto-freeze', state.frozen && state.frozen_reason === 'AUTO_FREEZE_TOO_MANY_BLOCKS', { tx: r.hash, detail: state.frozen_reason });
  await asOwner('unfreeze', {});
  await setPolicy({ auto_freeze_after_blocks: 0, denylist: [] });

  // ---------------------------------------------------------------- 16. auth checks
  r = await call(vendor, d.wallet, 'set_policy', { policy: baseline });
  expectTxFailure('a stranger cannot set the policy', r, 'ERR_NOT_OWNER');
  r = await call(vendor, d.wallet, 'pay', { receiver_id: d.vendor, amount: near('0.01'), purpose: 'stranger tries to pay itself' });
  expectTxFailure('a stranger cannot call pay', r, 'ERR_NOT_AGENT');
  r = await call(vendor, d.wallet, 'on_transfer_done', { payment_id: '1' });
  expectTxFailure('a stranger cannot fake a callback', r, 'is private');
  r = await call(vendor, d.wallet, 'withdraw', { amount: near('0.1'), to: d.vendor });
  expectTxFailure('a stranger cannot withdraw', r, 'ERR_NOT_OWNER');

  // ---------------------------------------------------------------- 17. protocol-level limits of the agent key
  await expectRejectedByProtocol('agent key cannot send a plain NEAR transfer', [actions.transfer(BigInt(near('0.01')))], d.drainer, 'RequiresFullAccess');
  // A transfer larger than the key's remaining allowance is refused even earlier, on the allowance check.
  await expectRejectedByProtocol('agent key cannot send more than its allowance either', [actions.transfer(BigInt(near('1')))], d.drainer, 'Allowance');
  await expectRejectedByProtocol('agent key cannot add a full-access key', [actions.addFullAccessKey(KeyPair.fromRandom('ed25519').getPublicKey())], d.wallet, 'RequiresFullAccess');
  await expectRejectedByProtocol('agent key cannot delete the account', [actions.deleteAccount(d.drainer)], d.wallet, 'RequiresFullAccess');
  await expectRejectedByProtocol('agent key cannot deploy code', [actions.deployContract(new Uint8Array([0, 97, 115, 109, 1, 0, 0, 0]))], d.wallet, 'RequiresFullAccess');
  await expectRejectedByProtocol('agent key cannot call owner methods (withdraw)', [actions.functionCall('withdraw', { amount: near('1'), to: d.drainer }, 30n * TGAS, 0n)], d.wallet, 'MethodNameMismatch');
  await expectRejectedByProtocol('agent key cannot call set_policy', [actions.functionCall('set_policy', { policy: baseline }, 30n * TGAS, 0n)], d.wallet, 'MethodNameMismatch');
  await expectRejectedByProtocol('agent key cannot attach a deposit', [actions.functionCall('pay', { receiver_id: d.vendor, amount: near('0.01'), purpose: 'deposit attempt test' }, 30n * TGAS, 1n)], d.wallet, 'DepositWithFunctionCall');
  await expectRejectedByProtocol('agent key cannot call another contract', [actions.functionCall('report', { account_id: d.vendor, reason: 'x', evidence_url: 'https://x.y' }, 30n * TGAS, 0n)], d.registry, 'ReceiverMismatch');

  // ---------------------------------------------------------------- 18. counterparty service
  const c1 = await checkCounterparty(d.drainer, near('0.05'), { wallet: d.wallet, registry: d.registry }, { dryRun: true });
  record('counterparty check blocks the drainer', c1.decision === 'block' && c1.reasons.some((x) => x.code === 'SHARED_BLOCKLIST'), { detail: c1.reasons.map((x) => x.code).join(',') });
  const c2 = await checkCounterparty(d.registry, near('0.01'), { wallet: d.wallet, registry: d.registry }, { dryRun: true });
  record('counterparty check flags a contract receiver', c2.signals.is_contract && c2.decision === 'block', { detail: c2.reasons.map((x) => x.code).join(',') });
  const c3 = await checkCounterparty(d.scouts[1], near('0.01'), { wallet: d.wallet, registry: d.registry }, { dryRun: true });
  record('counterparty check warns on a new, unknown receiver', c3.decision === 'warn', { detail: c3.reasons.map((x) => x.code).join(',') });

  // ---------------------------------------------------------------- cleanup
  await asOwner('set_policy', { policy: baseline });
  const hist = await view(d.wallet, 'get_history', { limit: 100 });
  const runStart = BigInt(t0) * 1_000_000n;
  const back = hist.filter((h) => h.status === 'paid' && h.receiver_id === d.vendor && BigInt(h.ts) >= runStart)
    .reduce((a, h) => a + BigInt(h.amount), 0n);
  if (back > 0n) {
    const rr = await send(vendor, d.wallet, [actions.transfer(back)]);
    console.log(`(vendor returned ${fmtNear(back)} NEAR to the wallet ${rr.url})`);
  }
  const perm2 = (await viewAccessKeys(d.wallet)).find((k) => k.public_key === agentPub)?.access_key.permission;
  console.log(`\nagent key allowance left: ${fmtNear(perm2.FunctionCall.allowance)} NEAR`);

  const summary = {
    ran_at: new Date().toISOString(),
    seconds: Math.round((Date.now() - t0) / 1000),
    passed: results.length - failed,
    failed,
    wallet: d.wallet,
    registry: d.registry,
    results: results.map((x) => ({ ...x, url: x.tx ? EXPLORER_TX + x.tx : undefined })),
  };
  fs.writeFileSync(path.join(ROOT, 'deployments', 'e2e-latest.json'), JSON.stringify(summary, null, 2) + '\n');
  console.log(`\n${summary.passed} passed, ${failed} failed in ${summary.seconds}s. Results: deployments/e2e-latest.json`);
  process.exit(failed ? 1 : 0);
}

main().catch((e) => { console.error('e2e crashed:', e.stack || e.message); process.exit(2); });
