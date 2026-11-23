#!/usr/bin/env node
// Installs Collate on a Mac or a Linux computer: `npx collate-app@latest`.
//
// On macOS it installs Xcode's command line tools when git is missing, then puts the newest
// release's Collate.app in Applications and a `collate` command in ~/.local/bin. On Linux it
// puts the binary in ~/.local/bin and Collate in the app launcher. With --build (or when the
// newest release has nothing to download for this computer) it builds the latest source
// instead, installing Rust first if it isn't there.
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
const LINUX = process.platform === 'linux';
const SOURCE = LINUX
  ? path.join(process.env.XDG_CACHE_HOME || path.join(HOME, '.cache'), 'collate', 'source')
  : path.join(HOME, 'Library', 'Caches', 'Collate', 'source');
/** The Linux download's architecture, as `uname -m` names it. */
const LINUX_ARCH = { x64: 'x86_64', arm64: 'aarch64' }[os.arch()];
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

function checkPlatform() {
  if (LINUX) {
    if (!LINUX_ARCH) fail(`Collate runs on x86_64 and arm64 Linux; this computer is ${os.arch()}.`);
    if (!run('git', ['--version']).ok) fail('Collate needs git. Install it with your package manager and run this again.');
    say(`Linux on ${LINUX_ARCH}`);
    return;
  }
  if (process.platform !== 'darwin') fail('Collate runs on macOS and Linux only.');
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
  // Linux replaces the binary by renaming over it, which a running Collate doesn't mind.
  if (LINUX) return;
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

/** Installs the newest release's app. Returns false when that release has nothing to download for this computer. */
async function installRelease() {
  const res = await fetch(`https://api.github.com/repos/${REPO}/releases/latest`, { headers: { 'User-Agent': 'collate-app', Accept: 'application/vnd.github+json' } });
  if (!res.ok) fail(`couldn't ask GitHub for the newest release: ${res.status} ${res.statusText}`);
  const release = await res.json();
  const suffix = LINUX ? `-linux-${LINUX_ARCH}.tar.gz` : '-macos-universal.zip';
  const zip = release.assets.find(a => a.name.endsWith(suffix));
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

    if (LINUX) {
      installTarball(file, work);
      say(`installed Collate ${release.tag_name.replace(/^v/, '')}`);
      return true;
    }

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

/** Unpacks a Linux download in `work` and runs the install.sh inside it. */
function installTarball(file, work) {
  step('tar', ['-xzf', file, '-C', work]);
  const folder = path.basename(file, '.tar.gz');
  step('sh', [path.join(work, folder, 'install.sh')]);
}

/** `collate` from a terminal: the same script scripts/install.sh writes. On its own or with a
 * folder it opens the app on that repository; with anything else (two files or folders, a
 * patch, `git diff | collate`, flags) it runs the app here and waits for its window. */
function writeCommand() {
  fs.mkdirSync(BIN, { recursive: true });
  fs.writeFileSync(path.join(BIN, 'collate'), `#!/bin/sh
# Opens Collate: \`collate [folder]\` on a repository, \`collate a b\` compares two files or
# folders, \`collate x.patch\` or \`git diff | collate\` reads a patch.
if [ -t 0 ] && { [ $# -eq 0 ] || { [ $# -eq 1 ] && [ -d "$1" ]; }; }; then
  dir=$(cd "\${1:-.}" 2>/dev/null && pwd) || { echo "collate: no such folder: \${1:-.}" >&2; exit 1; }
  exec open -n -a Collate --args "$dir"
fi
exec "${path.join(appsFolder(), 'Collate.app', 'Contents', 'MacOS', 'Collate')}" "$@"
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
  if (!LINUX) return step(path.join(SOURCE, 'scripts', 'install.sh'), [], { cwd: SOURCE, env });
  say('the build needs the X11, Wayland and fontconfig headers: on Debian or Ubuntu, `sudo apt install libxkbcommon-dev libwayland-dev libx11-dev libxcb1-dev libfontconfig-dev pkg-config`');
  step(path.join(SOURCE, 'scripts', 'package-linux.sh'), [], { cwd: SOURCE, env: { ...env, CARGO_TARGET_DIR: path.join(SOURCE, 'target') } });
  const work = fs.mkdtempSync(path.join(os.tmpdir(), 'collate-'));
  try {
    const dist = path.join(SOURCE, 'dist');
    const tarball = fs.readdirSync(dist).find(n => n.endsWith(`-linux-${LINUX_ARCH}.tar.gz`));
    if (!tarball) fail(`the build left no Linux download in ${dist}`);
    installTarball(path.join(dist, tarball), work);
  } finally {
    fs.rmSync(work, { recursive: true, force: true });
  }
}

/** Adds ~/.local/bin to the shell's PATH so `collate` works in a new terminal. */
function ensurePath() {
  if ((process.env.PATH || '').split(':').includes(BIN)) return;
  const shell = path.basename(process.env.SHELL || 'zsh');
  const profile = path.join(HOME, shell === 'bash' ? (LINUX ? '.bashrc' : '.bash_profile') : shell === 'fish' ? '.config/fish/config.fish' : '.zshrc');
  const line = shell === 'fish' ? 'fish_add_path $HOME/.local/bin' : 'export PATH="$HOME/.local/bin:$PATH"';
  const current = fs.existsSync(profile) ? fs.readFileSync(profile, 'utf8') : '';
  if (!current.includes('.local/bin')) {
    fs.mkdirSync(path.dirname(profile), { recursive: true });
    fs.appendFileSync(profile, `\n# Added by the Collate installer, for the \`collate\` command.\n${line}\n`);
    say(`added ~/.local/bin to your PATH in ${profile.replace(HOME, '~')}; open a new terminal to use \`collate\``);
  }
}

async function uninstall() {
  if (LINUX) {
    const data = process.env.XDG_DATA_HOME || path.join(HOME, '.local', 'share');
    for (const f of [path.join(BIN, 'collate'), path.join(data, 'applications', 'collate.desktop'), path.join(data, 'icons', 'hicolor', 'scalable', 'apps', 'collate.svg')]) fs.rmSync(f, { force: true });
    fs.rmSync(SOURCE, { recursive: true, force: true });
    say(`removed Collate. Settings are still in ${path.join(process.env.XDG_CONFIG_HOME || path.join(HOME, '.config'), 'collate')} and data in ${path.join(data, 'collate')}; delete those folders to remove them too.`);
    return;
  }
  await quitCollate();
  for (const app of ['/Applications/Collate.app', path.join(HOME, 'Applications', 'Collate.app')]) fs.rmSync(app, { recursive: true, force: true });
  fs.rmSync(path.join(BIN, 'collate'), { force: true });
  fs.rmSync(SOURCE, { recursive: true, force: true });
  say('removed Collate. Settings are still in ~/Library/Application Support/Collate; delete that folder to remove them too.');
}

async function main() {
  const args = process.argv.slice(2);
  if (args.includes('--help') || args.includes('-h')) {
    console.log('Usage: npx collate-app@latest [--build | --uninstall]\n\n  (none)       install or update Collate from the newest release\n  --build      build and install the latest source (installs Rust if needed)\n  --uninstall  remove Collate and the collate command');
    return;
  }
  const unknown = args.filter(a => !['--build', '--uninstall'].includes(a));
  if (unknown.length) fail(`unknown option ${unknown[0]}; see --help`);
  checkPlatform();
  if (args.includes('--uninstall')) return uninstall();

  if (!LINUX) await ensureCommandLineTools();
  if (args.includes('--build')) await build();
  else if (!await installRelease()) {
    say('the newest release has nothing to download for this computer; building from source instead');
    await build();
  }
  ensurePath();
  if (LINUX) return say('done. Open Collate from your app launcher, or run `collate` in a repository.');
  run('open', ['-a', 'Collate']);
  say('done. Open Collate from Launchpad, or run `collate` in a repository.');
}

main().catch(e => fail(e.message));
