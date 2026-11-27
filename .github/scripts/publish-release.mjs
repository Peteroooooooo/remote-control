import fs from 'node:fs';
import path from 'node:path';
import crypto from 'node:crypto';

const args = new Set(process.argv.slice(2));
const tag = process.env.TAG || '';
const exePath = process.env.EXE_PATH || '';
const apkPath = (process.env.APK_PATH || '').trim();
const manifestOut = process.env.OUT_MANIFEST || 'latest.json';
const githubRepo = process.env.GITHUB_REPO || '';
const cnbRepo = (process.env.CNB_REPO || '').trim();
const signingKeyHex = (process.env.UPDATE_SIGNING_KEY || '').trim();
const cnbToken = (process.env.CNB_TOKEN || '').trim();
const notes = (process.env.NOTES || '').trim();

function requireEnv(name, val) {
  if (!val) throw new Error(`Missing required env: ${name}`);
  return val;
}

function buildManifest() {
  requireEnv('TAG', tag);
  requireEnv('EXE_PATH', exePath);
  requireEnv('GITHUB_REPO', githubRepo);
  requireEnv('UPDATE_SIGNING_KEY', signingKeyHex);

  const version = tag.replace(/^v/, '');
  const buf = fs.readFileSync(exePath);
  const sha256 = crypto.createHash('sha256').update(buf).digest('hex');

  const pkcs8Prefix = Buffer.from('302e020100300506032b657004220420', 'hex');
  const seed = Buffer.from(signingKeyHex, 'hex');
  if (seed.length !== 32) {
    throw new Error(`UPDATE_SIGNING_KEY must be 64 hex chars, got ${signingKeyHex.length}`);
  }
  const privKey = crypto.createPrivateKey({
    key: Buffer.concat([pkcs8Prefix, seed]),
    format: 'der',
    type: 'pkcs8',
  });

  const payload = Buffer.from(`remote-control-update-v1\n${version}\n${sha256}\n`, 'utf8');
  const sig = crypto.sign(null, payload, privKey).toString('hex');

  const urls = [];
  if (cnbRepo) {
    urls.push(`https://cnb.cool/${cnbRepo}/-/releases/download/${tag}/remote-control.exe`);
  }
  urls.push(`https://github.com/${githubRepo}/releases/download/${tag}/remote-control.exe`);

  const manifest = { version, sha256, notes, urls, sig };

  // Phone section. Android checks the APK's own signing certificate, so it needs no manifest
  // signature; the PC signature above covers only version + exe sha256, so this section can sit
  // beside it. versionCode must match apps/rc-android/app/build.gradle.kts.
  if (apkPath) {
    const apk = fs.readFileSync(apkPath);
    const parts = version.split('-')[0].split('.').map((p) => parseInt(p, 10) || 0);
    const code = Math.max(1, (parts[0] || 0) * 1_000_000 + (parts[1] || 0) * 1_000 + (parts[2] || 0));
    const name = path.basename(apkPath);
    const apkUrls = [];
    if (cnbRepo) apkUrls.push(`https://cnb.cool/${cnbRepo}/-/releases/download/${tag}/${name}`);
    apkUrls.push(`https://github.com/${githubRepo}/releases/download/${tag}/${name}`);
    manifest.android = {
      code,
      sha256: crypto.createHash('sha256').update(apk).digest('hex'),
      size: apk.length,
      urls: apkUrls,
    };
  }
  fs.writeFileSync(manifestOut, JSON.stringify(manifest, null, 2) + '\n', 'utf8');
  console.log(`Generated ${manifestOut}: version=${version}, sha256=${sha256}`);
}

async function cnbFetch(urlPath, opts = {}) {
  const url = urlPath.startsWith('http') ? urlPath : `https://api.cnb.cool${urlPath}`;
  const headers = {
    Accept: 'application/vnd.cnb.api+json',
    Authorization: `Bearer ${cnbToken}`,
    ...(opts.headers || {}),
  };
  const res = await fetch(url, { ...opts, headers });
  const text = await res.text();
  if (!res.ok) {
    const err = new Error(`CNB ${opts.method || 'GET'} ${url} -> ${res.status}: ${text}`);
    err.status = res.status;
    throw err;
  }
  return text ? JSON.parse(text) : null;
}

async function defaultBranch() {
  const branches = await cnbFetch(`/${cnbRepo}/-/git/branches`);
  if (!Array.isArray(branches) || branches.length === 0) {
    throw new Error(`CNB repo ${cnbRepo} has no branches; initialize it with at least one commit (e.g. README)`);
  }
  const names = branches.map((b) => b.name);
  return names.includes('main') ? 'main' : names.includes('master') ? 'master' : names[0];
}

async function ensureRelease(relTag, title, branch, makeLatest) {
  try {
    return await cnbFetch(`/${cnbRepo}/-/releases/tags/${encodeURIComponent(relTag)}`);
  } catch (e) {
    if (e.status !== 404) throw e;
  }
  return await cnbFetch(`/${cnbRepo}/-/releases`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({
      tag_name: relTag,
      name: title,
      body: relTag === 'latest' ? 'Latest update manifest pointer' : notes || `Release ${relTag}`,
      target_commitish: branch,
      make_latest: makeLatest ? 'true' : 'false',
      draft: false,
      prerelease: false,
    }),
  });
}

async function uploadAsset(releaseId, filePath, assetName) {
  const data = fs.readFileSync(filePath);
  const info = await cnbFetch(`/${cnbRepo}/-/releases/${releaseId}/asset-upload-url`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({
      asset_name: assetName,
      size: data.length,
      overwrite: true,
    }),
  });

  const putRes = await fetch(info.upload_url, {
    method: 'PUT',
    body: data,
  });
  if (!putRes.ok) {
    throw new Error(`PUT asset ${assetName} failed: ${putRes.status} ${await putRes.text()}`);
  }

  await cnbFetch(info.verify_url, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ ttl: 0 }),
  });
  console.log(`Uploaded ${assetName} (${data.length} bytes) to CNB release ${releaseId}`);
}

async function syncCnb() {
  if (!cnbRepo || !cnbToken) {
    console.log('Skipping CNB sync (CNB_REPO or CNB_TOKEN not set).');
    return;
  }
  requireEnv('TAG', tag);
  requireEnv('EXE_PATH', exePath);

  const branch = await defaultBranch();
  const rel = await ensureRelease(tag, tag, branch, true);
  await uploadAsset(rel.id, exePath, path.basename(exePath));
  if (apkPath) await uploadAsset(rel.id, apkPath, path.basename(apkPath));
  await uploadAsset(rel.id, manifestOut, 'latest.json');

  const latestRel = await ensureRelease('latest', 'latest', branch, false);
  await uploadAsset(latestRel.id, manifestOut, 'latest.json');
}

if (args.has('--manifest')) buildManifest();
if (args.has('--sync-cnb')) await syncCnb();
