// One-time testnet setup. Idempotent: every step checks chain state first.
//
//   node scripts/setup.mjs
//
// Creates (testnet only):
//   OWNER     leash-busel.testnet               the human (faucet account)
//   WALLET    wallet.leash-busel.testnet        the Leash wallet contract (agent's money)
//   VENDOR    vendor.leash-busel.testnet        a legitimate receiver
//   DRAINER   drainer.leash-busel.testnet       the attacker's "known bad" address
//   COMMONS   leash-busel-commons.testnet       operator of the shared blocklist (faucet)
//   REGISTRY  registry.leash-busel-commons.testnet   the blocklist registry contract
//   SCOUT1/2  scout-1/scout-2.leash-busel-commons.testnet   two independent reporters
//
// Keys are written only to .env.testnet (gitignored). Nothing secret is printed.
import fs from 'node:fs';
import path from 'node:path';
import {
  ROOT, readEnv, appendEnv, writeDeployments, accountWithKey, newKeyPair, viewAccount,
  viewAccessKeys, view, send, call, actions, near, fmtNear, TGAS, EXPLORER_ACCOUNT,
} from '../agent/near.mjs';

const OWNER_ID = process.env.LEASH_OWNER || 'leash-busel.testnet';
const COMMONS_ID = process.env.LEASH_COMMONS || 'leash-busel-commons.testnet';
const WALLET_ID = `wallet.${OWNER_ID}`;
const VENDOR_ID = `vendor.${OWNER_ID}`;
const DRAINER_ID = `drainer.${OWNER_ID}`;
const REGISTRY_ID = `registry.${COMMONS_ID}`;
const SCOUT1_ID = `scout-1.${COMMONS_ID}`;
const SCOUT2_ID = `scout-2.${COMMONS_ID}`;

const WALLET_WASM = path.join(ROOT, 'res', 'leash_wallet.wasm');
const REGISTRY_WASM = path.join(ROOT, 'res', 'leash_registry.wasm');

export const DEMO_POLICY = {
  per_payment_cap: near('0.1'),
  daily_cap: near('0.5'),
  per_receiver_daily_cap: near('0.3'),
  max_payments_per_hour: 30,
  allowlist_mode: false,
  allowlist: [],
  denylist: [],
  contracts_require_allowlist: true,
  known_contracts: [],
  min_purpose_len: 8,
  registry: REGISTRY_ID,
  auto_freeze_after_blocks: 0,
  guardians: [],
};

const AGENT_ALLOWANCE = near('0.5');

function log(...a) { console.log(...a); }

async function faucetAccount(id, keyName) {
  let env = readEnv();
  if (await viewAccount(id)) { log(`= ${id} exists`); return; }
  if (env[keyName]) throw new Error(`${keyName} is set but ${id} does not exist on chain`);
  const kp = newKeyPair();
  const r = await fetch('https://helper.nearprotocol.com/account', {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ newAccountId: id, newAccountPublicKey: kp.getPublicKey().toString() }),
  });
  if (!r.ok) throw new Error(`faucet refused ${id}: ${r.status} ${(await r.text()).slice(0, 200)}`);
  appendEnv({ [keyName]: kp.toString() });
  for (let i = 0; i < 10 && !(await viewAccount(id)); i++) await new Promise((s) => setTimeout(s, 1500));
  log(`+ ${id} created by the testnet faucet`);
}

async function subAccount(parent, id, keyName, amount) {
  if (await viewAccount(id)) { log(`= ${id} exists`); return; }
  const kp = newKeyPair();
  const r = await send(parent, id, [
    actions.createAccount(),
    actions.transfer(BigInt(amount)),
    actions.addFullAccessKey(kp.getPublicKey()),
  ]);
  if (!r.ok) throw new Error(`create ${id} failed: ${JSON.stringify(r.failure)}`);
  appendEnv({ [keyName]: kp.toString() });
  log(`+ ${id} created with ${fmtNear(amount, 2)} NEAR  ${r.url}`);
}

async function deploy(account, wasmPath, initMethod, initArgs) {
  const st = await viewAccount(account.accountId);
  if (st.code_hash !== '11111111111111111111111111111111') {
    log(`= ${account.accountId} already has code ${st.code_hash}`);
    return null;
  }
  const code = fs.readFileSync(wasmPath);
  const r = await send(account, account.accountId, [
    actions.deployContract(new Uint8Array(code)),
    actions.functionCall(initMethod, initArgs, 100n * TGAS, 0n),
  ]);
  if (!r.ok) throw new Error(`deploy ${account.accountId} failed: ${JSON.stringify(r.failure)}`);
  log(`+ deployed ${path.basename(wasmPath)} to ${account.accountId}  ${r.url}`);
  return r.hash;
}

async function main() {
  if (!fs.existsSync(WALLET_WASM) || !fs.existsSync(REGISTRY_WASM)) {
    throw new Error('Build the contracts first: npm run build');
  }
  await faucetAccount(OWNER_ID, 'OWNER_KEY');
  await faucetAccount(COMMONS_ID, 'COMMONS_KEY');
  let env = readEnv();
  const owner = accountWithKey(OWNER_ID, env.OWNER_KEY);
  const commons = accountWithKey(COMMONS_ID, env.COMMONS_KEY);

  await subAccount(owner, WALLET_ID, 'WALLET_FULL_KEY', near('4'));
  await subAccount(owner, VENDOR_ID, 'VENDOR_KEY', near('0.05'));
  await subAccount(owner, DRAINER_ID, 'DRAINER_KEY', near('0.05'));
  await subAccount(commons, REGISTRY_ID, 'REGISTRY_KEY', near('2.5'));
  await subAccount(commons, SCOUT1_ID, 'SCOUT1_KEY', near('0.3'));
  await subAccount(commons, SCOUT2_ID, 'SCOUT2_KEY', near('0.3'));
  env = readEnv();

  const tx = {};
  const registryAcc = accountWithKey(REGISTRY_ID, env.REGISTRY_KEY);
  tx.deploy_registry = await deploy(registryAcc, REGISTRY_WASM, 'new', {
    owner_id: COMMONS_ID, threshold: 2, report_deposit: near('0.01'),
  });
  const walletAcc = accountWithKey(WALLET_ID, env.WALLET_FULL_KEY);
  tx.deploy_wallet = await deploy(walletAcc, WALLET_WASM, 'new', {
    owner_id: OWNER_ID, policy: DEMO_POLICY,
  });

  // Agent key: a function-call key on the wallet account, added by the OWNER through the
  // contract (add_agent_key), so it is scoped to pay/request_payment with a gas allowance.
  if (!env.AGENT_KEY) {
    const kp = newKeyPair();
    const r = await call(owner, WALLET_ID, 'add_agent_key', {
      public_key: kp.getPublicKey().toString(), allowance: AGENT_ALLOWANCE,
    });
    if (!r.ok) throw new Error(`add_agent_key failed: ${JSON.stringify(r.failure)}`);
    appendEnv({ AGENT_KEY: kp.toString() });
    tx.add_agent_key = r.hash;
    log(`+ agent function-call key added via wallet.add_agent_key  ${r.url}`);
    env = readEnv();
  } else {
    log('= agent key already present');
  }

  // The registry operator makes COMMONS an attester (it is the owner, so it already can).
  const cfg = await view(REGISTRY_ID, 'get_config');
  log(`= registry threshold ${cfg.threshold}, report deposit ${fmtNear(cfg.report_deposit, 3)} NEAR`);

  const agentPub = (await import('near-api-js')).KeyPair.fromString(env.AGENT_KEY).getPublicKey().toString();
  const keys = await viewAccessKeys(WALLET_ID);
  const agentKey = keys.find((k) => k.public_key === agentPub);

  const deployments = {
    network: 'testnet',
    rpc: 'https://test.rpc.fastnear.com',
    owner: OWNER_ID,
    wallet: WALLET_ID,
    registry: REGISTRY_ID,
    registry_owner: COMMONS_ID,
    vendor: VENDOR_ID,
    drainer: DRAINER_ID,
    scouts: [SCOUT1_ID, SCOUT2_ID],
    agent_public_key: agentPub,
    agent_key_permission: agentKey?.access_key?.permission ?? null,
    setup_txs: tx,
    explorer: {
      wallet: EXPLORER_ACCOUNT + WALLET_ID,
      registry: EXPLORER_ACCOUNT + REGISTRY_ID,
    },
  };
  // Keep tx hashes from an earlier run if this run skipped those steps.
  const file = path.join(ROOT, 'deployments', 'testnet.json');
  if (fs.existsSync(file)) {
    const prev = JSON.parse(fs.readFileSync(file, 'utf8'));
    for (const [k, v] of Object.entries(prev.setup_txs || {})) if (!tx[k] && v) tx[k] = v;
  }
  writeDeployments(deployments);
  fs.writeFileSync(path.join(ROOT, 'dashboard', 'config.js'), [
    '// Written by scripts/setup.mjs from deployments/testnet.json. Override in the URL with',
    '// ?wallet=<account>&registry=<account>&rpc=<url>.',
    'export const CONFIG = {',
    `  wallet: '${WALLET_ID}',`,
    `  registry: '${REGISTRY_ID}',`,
    "  rpcs: ['https://test.rpc.fastnear.com', 'https://rpc.testnet.near.org'],",
    '};',
    '',
  ].join('\n'));
  log('\nAgent key permission on chain:');
  log(JSON.stringify(deployments.agent_key_permission, null, 2));
  log(`\nWrote deployments/testnet.json. Wallet: ${EXPLORER_ACCOUNT}${WALLET_ID}`);
}

main().catch((e) => { console.error('setup failed:', e.message); process.exit(1); });
