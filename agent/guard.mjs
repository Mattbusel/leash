// Agent-side wrapper. The agent never calls the wallet contract directly: it calls
// guardedPay(), which runs the counterparty check first.
//   block -> refuse to sign, log why
//   warn  -> ask a human: a terminal prompt when interactive, otherwise an approval
//            queue file (demo/state/approvals.json) that a human resolves with
//            `node agent/cli.mjs approvals` / `approve-warning <id>`
//   allow -> sign `pay` with the agent's function-call key
import fs from 'node:fs';
import path from 'node:path';
import crypto from 'node:crypto';
import readline from 'node:readline/promises';
import { ROOT, call, readEnv, readDeployments, accountWithKey, fmtNear } from './near.mjs';
import { checkCounterparty, formatCheck } from './counterparty.mjs';

export const QUEUE_FILE = path.join(ROOT, 'demo', 'state', 'approvals.json');

export function loadQueue() {
  try { return JSON.parse(fs.readFileSync(QUEUE_FILE, 'utf8')); } catch { return []; }
}
export function saveQueue(q) {
  fs.mkdirSync(path.dirname(QUEUE_FILE), { recursive: true });
  fs.writeFileSync(QUEUE_FILE, JSON.stringify(q, null, 2) + '\n');
}

function intentKey({ receiver_id, amount, purpose }) {
  return crypto.createHash('sha256').update(`${receiver_id}|${amount}|${purpose}`).digest('hex').slice(0, 12);
}

async function askHuman(check, intent, mode) {
  const key = intentKey(intent);
  const q = loadQueue();
  const existing = q.find((e) => e.id === key);
  if (existing?.status === 'approved') return { approved: true, via: `approval queue (${key})` };
  if (existing?.status === 'denied') return { approved: false, via: `approval queue (${key})` };

  if (mode === 'prompt' && process.stdin.isTTY) {
    const rl = readline.createInterface({ input: process.stdin, output: process.stdout });
    const ans = await rl.question(`Human approval needed for ${fmtNear(intent.amount)} NEAR to ${intent.receiver_id}. Approve? [y/N] `);
    rl.close();
    return { approved: /^y(es)?$/i.test(ans.trim()), via: 'terminal prompt' };
  }
  if (mode === 'deny') return { approved: false, via: 'LEASH_WARN_MODE=deny' };

  // Queue mode: record it and refuse for now. The human approves it out of band and the
  // agent's retry of the same intent goes through.
  if (!existing) {
    q.push({
      id: key,
      status: 'waiting',
      created_at: new Date().toISOString(),
      intent,
      reasons: check.reasons.filter((r) => r.level !== 'info'),
    });
    saveQueue(q);
  }
  return { approved: false, via: `approval queue (${key}), waiting for a human`, queued: key };
}

/**
 * Pay through the guard. Returns { stage, check, tx? }.
 * stage: refused | waiting_for_human | denied_by_human | submitted
 */
export async function guardedPay(intent, { mode = process.env.LEASH_WARN_MODE || 'prompt', log = console.log } = {}) {
  const d = readDeployments();
  const env = readEnv();
  const amount = String(intent.amount);
  const full = { receiver_id: intent.receiver_id, amount, purpose: intent.purpose || '' };

  const check = await checkCounterparty(full.receiver_id, amount, { wallet: d.wallet, registry: d.registry });
  log(formatCheck(check));

  if (check.decision === 'block') {
    log('[guard] REFUSED to sign: counterparty check returned block.');
    return { stage: 'refused', check };
  }
  if (check.decision === 'warn') {
    const h = await askHuman(check, full, mode);
    log(`[guard] human decision via ${h.via}: ${h.approved ? 'approved' : 'not approved'}`);
    if (!h.approved) return { stage: h.queued ? 'waiting_for_human' : 'denied_by_human', check, queued: h.queued };
  }

  const agent = accountWithKey(d.wallet, env.AGENT_KEY);
  log(`[guard] signing wallet.pay with the agent function-call key (${d.agent_public_key.slice(0, 16)}...)`);
  const tx = await call(agent, d.wallet, 'pay', full, { gasTgas: 100 });
  const outcome = tx.value;
  log(`[chain] ${outcome?.status ?? (tx.ok ? 'ok' : 'failed')}${outcome?.reason ? ' ' + outcome.reason : ''}  ${tx.url}`);
  return { stage: 'submitted', check, tx };
}
