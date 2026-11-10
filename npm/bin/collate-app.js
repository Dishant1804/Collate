#!/usr/bin/env node
// Installs Collate on this Mac: `npx collate-app@latest`.
//
// Checks the Mac, installs Xcode's command line tools when git is missing, then puts the newest
// release's Collate.app in Applications and a `collate` command in ~/.local/bin. With --build
// (or when the newest release has no app to download) it builds the latest source instead,
// installing Rust first if it isn't there.
//
//   npx collate-app@latest              install or update to the newest release
//   npx collate-app@latest --build      build and install the latest source
//   npx collate-app@latest --uninstall  remove the app and the command (keeps settings)
'use strict';

const { spawnSync } = require('node:child_process');
const crypto = require('node:crypto');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');

const REPO = 'Dishant1804/Collate';
const HOME = os.homedir();
const BIN = path.join(HOME, '.local', 'bin');
const SOURCE = path.join(HOME, 'Library', 'Caches', 'Collate', 'source');
const CARGO_BIN = path.join(HOME, '.cargo', 'bin');
const LSREGISTER = '/System/Library/Frameworks/CoreServices.framework/Versions/A/Frameworks/LaunchServices.framework/Versions/A/Support/lsregister';

function say(text) { console.log(`collate: ${text}`); }
function fail(text) { console.error(`collate: ${text}`); process.exit(1); }

/** Runs a command, quietly, and returns whether it succeeded and what it printed. */
function run(cmd, args, opts = {}) {
  const r = spawnSync(cmd, args, { encoding: 'utf8', ...opts });
  return { ok: r.status === 0, out: (r.stdout || '').trim(), err: (r.stderr || '').trim() || (r.error && r.error.message) || '' };
}

/** Runs a command with its output shown, and stops the install if it fails. */
function step(cmd, args, opts = {}) {
  const r = spawnSync(cmd, args, { stdio: 'inherit', ...opts });
  if (r.status !== 0) fail(`\`${[cmd, ...args].join(' ')}\` failed${r.error ? `: ${r.error.message}` : ''}`);
}

function sleep(ms) { return new Promise(resolve => setTimeout(resolve, ms)); }

function checkMac() {
  if (process.platform !== 'darwin') fail('Collate runs on macOS only.');
  const version = run('sw_vers', ['-productVersion']).out;
  if (Number(version.split('.')[0]) < 13) fail(`Collate needs macOS 13 or newer; this Mac has ${version || 'an unknown version'}.`);
  say(`macOS ${version} on ${os.arch() === 'arm64' ? 'Apple silicon' : 'Intel'}`);
}

/** Collate runs git for everything, and a build needs the compiler and linker: both come with Xcode's command line tools. */
async function ensureCommandLineTools() {
  if (run('xcode-select', ['-p']).ok && run('git', ['--version']).ok) return;
  say('installing Xcode\'s command line tools (git, the compiler). Finish the dialog that opens.');
  run('xcode-select', ['--install']);
  while (!(run('xcode-select', ['-p']).ok && run('git', ['--version']).ok)) await sleep(5000);
  say('command line tools installed');
}

/** The installer can't replace an app that is running. */
async function quitCollate() {
  if (!run('pgrep', ['-x', 'Collate']).ok) return;
  say('quitting Collate to replace it');
  run('osascript', ['-e', 'quit app "Collate"']);
  for (let i = 0; i < 20 && run('pgrep', ['-x', 'Collate']).ok; i++) await sleep(500);
  if (run('pgrep', ['-x', 'Collate']).ok) fail('Collate is still running. Quit it and run this again.');
}

function appsFolder() {
  try { fs.accessSync('/Applications', fs.constants.W_OK); return '/Applications'; }
  catch { return path.join(HOME, 'Applications'); }
}

async function download(url, file) {
  const res = await fetch(url, { headers: { 'User-Agent': 'collate-app' } });
  if (!res.ok) fail(`couldn't download ${url}: ${res.status} ${res.statusText}`);
  fs.writeFileSync(file, Buffer.from(await res.arrayBuffer()));
}

/** Installs the newest release's app. Returns false when that release has no app to download. */
async function installRelease() {
  const res = await fetch(`https://api.github.com/repos/${REPO}/releases/latest`, { headers: { 'User-Agent': 'collate-app', Accept: 'application/vnd.github+json' } });
  if (!res.ok) fail(`couldn't ask GitHub for the newest release: ${res.status} ${res.statusText}`);
  const release = await res.json();
  const zip = release.assets.find(a => a.name.endsWith('-macos-universal.zip'));
  const sums = release.assets.find(a => a.name === 'SHA256SUMS');
  if (!zip || !sums) return false;

  say(`downloading Collate ${release.tag_name.replace(/^v/, '')}`);
  const work = fs.mkdtempSync(path.join(os.tmpdir(), 'collate-'));
  try {
    const file = path.join(work, zip.name);
    await download(zip.browser_download_url, file);
    await download(sums.browser_download_url, path.join(work, 'SHA256SUMS'));
    const want = fs.readFileSync(path.join(work, 'SHA256SUMS'), 'utf8').split('\n').map(l => l.split(/\s+/)).find(([, name]) => name === zip.name);
    const got = crypto.createHash('sha256').update(fs.readFileSync(file)).digest('hex');
    if (!want || want[0] !== got) fail(`${zip.name} doesn't match its checksum in SHA256SUMS; not installing it.`);

    step('ditto', ['-x', '-k', file, work]);
    await quitCollate();
    const apps = appsFolder();
    fs.mkdirSync(apps, { recursive: true });
    fs.rmSync(path.join(apps, 'Collate.app'), { recursive: true, force: true });
    step('ditto', [path.join(work, 'Collate.app'), path.join(apps, 'Collate.app')]);
    run(LSREGISTER, ['-f', path.join(apps, 'Collate.app')]);
    writeCommand();
    say(`installed ${path.join(apps, 'Collate.app')} (${release.tag_name.replace(/^v/, '')}) and ${path.join(BIN, 'collate')}`);
  } finally {
    fs.rmSync(work, { recursive: true, force: true });
  }
  return true;
}

/** `collate [path]` from a terminal opens the app on that repository; the same script scripts/install.sh writes. */
function writeCommand() {
  fs.mkdirSync(BIN, { recursive: true });
  fs.writeFileSync(path.join(BIN, 'collate'), `#!/bin/sh
# Opens Collate on a repository: \`collate\` (the current folder) or \`collate <path>\`.
dir=$(cd "\${1:-.}" 2>/dev/null && pwd) || { echo "collate: no such folder: \${1:-.}" >&2; exit 1; }
exec open -n -a Collate --args "$dir"
`, { mode: 0o755 });
}

/** Builds the latest source with scripts/install.sh, installing Rust first when it's missing. */
async function build() {
  const env = { ...process.env, PATH: `${CARGO_BIN}:${process.env.PATH}` };
  if (!run('cargo', ['--version'], { env }).ok) {
    say('installing Rust (rustup)');
    step('/bin/sh', ['-c', 'curl --proto \'=https\' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y']);
  }
  if (fs.existsSync(path.join(SOURCE, '.git'))) {
    say('fetching the latest source');
    step('git', ['-C', SOURCE, 'fetch', '--quiet', 'origin']);
    step('git', ['-C', SOURCE, 'reset', '--quiet', '--hard', 'origin/HEAD']);
  } else {
    say('downloading the source');
    fs.mkdirSync(path.dirname(SOURCE), { recursive: true });
    step('git', ['clone', '--quiet', `https://github.com/${REPO}.git`, SOURCE]);
  }
  say('building (the first build takes a few minutes)');
  await quitCollate();
  step(path.join(SOURCE, 'scripts', 'install.sh'), [], { cwd: SOURCE, env });
}

/** Adds ~/.local/bin to the shell's PATH so `collate` works in a new terminal. */
function ensurePath() {
  if ((process.env.PATH || '').split(':').includes(BIN)) return;
  const shell = path.basename(process.env.SHELL || 'zsh');
  const profile = path.join(HOME, shell === 'bash' ? '.bash_profile' : shell === 'fish' ? '.config/fish/config.fish' : '.zshrc');
  const line = shell === 'fish' ? 'fish_add_path $HOME/.local/bin' : 'export PATH="$HOME/.local/bin:$PATH"';
  const current = fs.existsSync(profile) ? fs.readFileSync(profile, 'utf8') : '';
  if (!current.includes('.local/bin')) {
    fs.mkdirSync(path.dirname(profile), { recursive: true });
    fs.appendFileSync(profile, `\n# Added by the Collate installer, for the \`collate\` command.\n${line}\n`);
    say(`added ~/.local/bin to your PATH in ${profile.replace(HOME, '~')}; open a new terminal to use \`collate\``);
  }
}

async function uninstall() {
  await quitCollate();
  for (const app of ['/Applications/Collate.app', path.join(HOME, 'Applications', 'Collate.app')]) fs.rmSync(app, { recursive: true, force: true });
  fs.rmSync(path.join(BIN, 'collate'), { force: true });
  fs.rmSync(SOURCE, { recursive: true, force: true });
  say('removed Collate. Settings are still in ~/Library/Application Support/Collate; delete that folder to remove them too.');
}

async function main() {
  const args = process.argv.slice(2);
  if (args.includes('--help') || args.includes('-h')) {
    console.log('Usage: npx collate-app@latest [--build | --uninstall]\n\n  (none)       install or update Collate from the newest release\n  --build      build and install the latest source (installs Rust if needed)\n  --uninstall  remove Collate.app and the collate command');
    return;
  }
  const unknown = args.filter(a => !['--build', '--uninstall'].includes(a));
  if (unknown.length) fail(`unknown option ${unknown[0]}; see --help`);
  checkMac();
  if (args.includes('--uninstall')) return uninstall();

  await ensureCommandLineTools();
  if (args.includes('--build')) await build();
  else if (!await installRelease()) {
    say('the newest release has no app to download; building from source instead');
    await build();
  }
  ensurePath();
  run('open', ['-a', 'Collate']);
  say('done. Open Collate from Launchpad, or run `collate` in a repository.');
}

main().catch(e => fail(e.message));
