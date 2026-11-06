// Prepare the runner's throwaway checkout for an unsigned release build:
//
//  1. Stamp the version the workflow resolved (optional first argument). The tag
//     is the release counter — tauri.conf.json in the repository keeps upstream's
//     version, so syncing from upstream never fights a build number.
//  2. Strip the one thing a GitHub runner cannot satisfy: Authenticode signing
//     (`bundle.windows.certificateThumbprint`) needs the author's
//     Certum/SimplySign token in the runner's certificate store. Without it
//     `signtool` fails — AFTER the 12-minute Rust compile, which is how this file
//     came to exist.
//
// It edits the files IN PLACE, and only on the runner: `actions/checkout` hands
// CI a disposable copy, so nothing is committed back. A separate `--config`
// patch would have been neater, but `--config` MERGES into the base config and a
// key that is merely absent from the patch keeps the base value — so the only
// way to be sure the thumbprint is gone is to not have it in the file at all.
//
// This build carries no updater at all (the fork removed it), so there is no
// updater artifact to sign either. Everything else — the NSIS target, its
// languages, the installer hooks, the icons — is left exactly as the repository
// has it.
import { readFileSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const repoRoot = join(dirname(fileURLToPath(import.meta.url)), '..', '..');
const configPath = join(repoRoot, 'src-tauri', 'tauri.conf.json');
const cargoPath = join(repoRoot, 'src-tauri', 'Cargo.toml');

const version = (process.argv[2] ?? '').trim();

const config = JSON.parse(readFileSync(configPath, 'utf8'));

if (version) {
  config.version = version;
  // The crate version is what Windows shows in the exe's properties; the bundle
  // takes its own from the config above, so the two have to agree or the
  // installer and the program it installs would disagree about what they are.
  const cargo = readFileSync(cargoPath, 'utf8');
  const stamped = cargo.replace(/^(version\s*=\s*")[^"]*(")/m, `$1${version}$2`);
  writeFileSync(cargoPath, stamped, 'utf8');
}

const windows = { ...(config.bundle?.windows ?? {}) };
for (const key of ['certificateThumbprint', 'signCommand', 'digestAlgorithm', 'timestampUrl', 'tsp']) {
  delete windows[key];
}
config.bundle = { ...(config.bundle ?? {}), windows };

writeFileSync(configPath, `${JSON.stringify(config, null, 2)}\n`, 'utf8');
console.log(`unsigned CI config written to ${configPath}${version ? ` at v${version}` : ''}`);
