#!/usr/bin/env node
// Leash command line. Agent commands sign with the agent's function-call key; owner
// commands sign with the owner's own account key. Testnet only.
//
//   node agent/cli.mjs status
//   node agent/cli.mjs check <receiver> [amountNEAR]
//   node agent/cli.mjs pay <receiver> <amountNEAR> <purpose...>      (guarded, agent key)
//   node agent/cli.mjs pending | history [n] | blocklist | keys | policy
//   node agent/cli.mjs approve <id> | reject <id> [note]               (owner)
//   node agent/cli.mjs freeze [reason] | unfreeze                      (owner)
//   node agent/cli.mjs allow <account> | deny <account> | unlist <account>   (owner policy edits)
//   node agent/cli.mjs set-policy <file.json>                          (owner)
//   node agent/cli.mjs withdraw <amountNEAR>                           (owner)
//   node agent/cli.mjs report <account> <evidenceUrl> <reason...> [--as scout1|scout2|owner]
//   node agent/cli.mjs approvals | approve-warning <id> | deny-warning <id>
import {
  readEnv, readDeployments, accountWithKey, view, call, near, fmtNear, viewAccessKeys,
} from './near.mjs';
import { checkCounterparty, formatCheck } from './counterparty.mjs';
import { guardedPay, loadQueue, saveQueue } from './guard.mjs';
import fs from 'node:fs';

const d = readDeployments();
const env = readEnv();
const [cmd, ...args] = process.argv.slice(2);

const owner = () => accountWithKey(d.owner, env.OWNER_KEY);
const ts = (ns) => new Date(Number(BigInt(ns) / 1_000_000n)).toISOString().replace('.000Z', 'Z');

function printTx(r) {
  const v = r.value ? ` -> ${JSON.stringify(r.value)}` : '';
  console.log(`${r.ok ? 'ok' : 'FAILED'}${v}\n${r.url}`);
  for (const e of r.events) console.log(`  event ${e.event}: ${JSON.stringify(e.data[0])}`);
  if (!r.ok) console.log('  failure:', JSON.stringify(r.failure));
}

async function editPolicy(fn) {
  const p = await view(d.wallet, 'get_policy');
  fn(p);
  printTx(await call(owner(), d.wallet, 'set_policy', { policy: p }));
}

const commands = {
  async status() {
    const [s, sp, p] = await Promise.all([
      view(d.wallet, 'get_state'), view(d.wallet, 'get_spend'), view(d.wallet, 'get_policy'),
    ]);
    console.log(`wallet   ${d.wallet}   owner ${s.owner_id}`);
    console.log(`frozen   ${s.frozen}${s.frozen_reason ? ' (' + s.frozen_reason + ')' : ''}`);
    console.log(`balance  ${fmtNear(s.balance)} NEAR, available to pay ${fmtNear(s.available)} NEAR`);
    console.log(`24h      spent ${fmtNear(sp.spent_24h)} of ${fmtNear(sp.daily_cap)} NEAR, in flight ${fmtNear(sp.in_flight)}`);
    console.log(`hour     ${sp.attempts_last_hour}/${sp.max_payments_per_hour} attempts, ${sp.blocked_last_hour} blocked`);
    for (const [a, v] of sp.per_receiver) console.log(`         ${a}: ${fmtNear(v)} of ${fmtNear(sp.per_receiver_daily_cap)} NEAR`);
    console.log(`caps     per payment ${fmtNear(p.per_payment_cap)}, per receiver/24h ${fmtNear(p.per_receiver_daily_cap)}`);
    console.log(`lists    allowlist_mode=${p.allowlist_mode} allow=[${p.allowlist}] deny=[${p.denylist}]`);
    console.log(`registry ${p.registry}`);
    console.log(`totals   paid ${s.paid_count} (${fmtNear(s.paid_total)} NEAR), blocked ${s.blocked_count}, pending ${s.pending_count}`);
  },
  async policy() { console.log(JSON.stringify(await view(d.wallet, 'get_policy'), null, 2)); },
  async check(receiver, amount = '0.01') {
    const c = await checkCounterparty(receiver, near(amount), { wallet: d.wallet, registry: d.registry });
    console.log(formatCheck(c));
    console.log(JSON.stringify(c.signals, null, 2));
  },
  async pay(receiver, amount, ...purpose) {
    if (!receiver || !amount) throw new Error('usage: pay <receiver> <amountNEAR> <purpose...>');
    await guardedPay({ receiver_id: receiver, amount: near(amount), purpose: purpose.join(' ') });
  },
  async pending() {
    const p = await view(d.wallet, 'get_pending', {});
    if (!p.length) console.log('no pending requests');
    for (const r of p) console.log(`#${r.id}  ${fmtNear(r.amount)} NEAR -> ${r.receiver_id}  [${r.reason}]  "${r.purpose}"  ${ts(r.created_at)}`);
  },
  async history(n = '20') {
    for (const h of await view(d.wallet, 'get_history', { limit: Number(n) })) {
      console.log(`#${String(h.id).padEnd(4)} ${ts(h.ts)}  ${h.status.padEnd(8)} ${fmtNear(h.amount)} -> ${h.receiver_id}  ${h.reason ?? ''}  "${h.purpose}"`);
    }
  },
  async approve(id) { printTx(await call(owner(), d.wallet, 'approve', { request_id: String(id) }, { gasTgas: 150 })); },
  async reject(id, ...note) { printTx(await call(owner(), d.wallet, 'reject', { request_id: String(id), note: note.join(' ') || null })); },
  async freeze(...reason) { printTx(await call(owner(), d.wallet, 'freeze', { reason: reason.join(' ') || null })); },
  async unfreeze() { printTx(await call(owner(), d.wallet, 'unfreeze', {})); },
  async allow(acct) { await editPolicy((p) => { if (!p.allowlist.includes(acct)) p.allowlist.push(acct); }); },
  async deny(acct) { await editPolicy((p) => { if (!p.denylist.includes(acct)) p.denylist.push(acct); }); },
  async unlist(acct) { await editPolicy((p) => { p.allowlist = p.allowlist.filter((a) => a !== acct); p.denylist = p.denylist.filter((a) => a !== acct); }); },
  async 'set-policy'(file) { printTx(await call(owner(), d.wallet, 'set_policy', { policy: JSON.parse(fs.readFileSync(file, 'utf8')) })); },
  async withdraw(amount) { printTx(await call(owner(), d.wallet, 'withdraw', { amount: near(amount), to: null })); },
  async report(account, url, ...rest) {
    let as = 'scout1';
    const i = rest.indexOf('--as');
    if (i >= 0) { as = rest[i + 1]; rest.splice(i, 2); }
    const who = { scout1: [d.scouts[0], env.SCOUT1_KEY], scout2: [d.scouts[1], env.SCOUT2_KEY], owner: [d.owner, env.OWNER_KEY] }[as];
    if (!who) throw new Error('--as must be scout1, scout2 or owner');
    const cfg = await view(d.registry, 'get_config');
    printTx(await call(accountWithKey(...who), d.registry, 'report', { account_id: account, reason: rest.join(' '), evidence_url: url }, { deposit: cfg.report_deposit }));
  },
  async blocklist() {
    for (const s of await view(d.registry, 'list_targets', {})) {
      console.log(`${s.blocked ? 'BLOCKED' : 'reported'}  ${s.account_id}  ${s.reports}/${s.threshold} reports${s.blocked_by ? ' by ' + s.blocked_by : ''}${s.contract_code_hash ? ' code ' + s.contract_code_hash : ''}`);
    }
  },
  async keys() {
    for (const k of await viewAccessKeys(d.wallet)) console.log(k.public_key, JSON.stringify(k.access_key.permission));
  },
  async approvals() {
    const q = loadQueue();
    if (!q.length) console.log('approval queue is empty');
    for (const e of q) console.log(`${e.id}  ${e.status.padEnd(8)} ${fmtNear(e.intent.amount)} -> ${e.intent.receiver_id}  ${e.reasons.map((r) => r.code).join(',')}`);
  },
  async 'approve-warning'(id) { setQueue(id, 'approved'); },
  async 'deny-warning'(id) { setQueue(id, 'denied'); },
};

function setQueue(id, status) {
  const q = loadQueue();
  const e = q.find((x) => x.id === id);
  if (!e) throw new Error(`no queued approval ${id}`);
  e.status = status;
  e.decided_at = new Date().toISOString();
  saveQueue(q);
  console.log(`${id} -> ${status}. Re-run the same payment and the guard will use this decision.`);
}

if (!commands[cmd]) {
  console.log(fs.readFileSync(new URL(import.meta.url), 'utf8').split('\n').filter((l) => l.startsWith('//')).join('\n'));
  process.exit(cmd ? 1 : 0);
}
commands[cmd](...args).catch((e) => { console.error('error:', e.message); process.exit(1); });
