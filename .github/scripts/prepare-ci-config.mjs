// Strip the two things a GitHub runner cannot satisfy from `tauri.conf.json`:
//
//  - Authenticode signing (`bundle.windows.certificateThumbprint`) needs the
//    author's Certum/SimplySign token plugged into the runner's certificate
//    store. Without it `signtool` fails — AFTER the 12-minute Rust compile,
//    which is how this file came to exist.
//  - Updater artifact signing (`bundle.createUpdaterArtifacts`) needs
//    `TAURI_SIGNING_PRIVATE_KEY`, a secret this fork does not have.
//
// It edits the file IN PLACE, and only on the runner: `actions/checkout` hands
// CI a disposable copy, so nothing is committed back. A separate `--config`
// patch would have been neater, but `--config` MERGES into the base config and
// a key that is merely absent from the patch keeps the base value — so the only
// way to be sure the thumbprint is gone is to not have it in the file at all.
//
// Everything else (NSIS target, its languages, the installer hooks, the icons)
// is left exactly as the repository has it. The result is a normal installer
// that simply isn't code-signed.
import { readFileSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const repoRoot = join(dirname(fileURLToPath(import.meta.url)), '..', '..');
const configPath = join(repoRoot, 'src-tauri', 'tauri.conf.json');

const config = JSON.parse(readFileSync(configPath, 'utf8'));

config.bundle = { ...(config.bundle ?? {}), createUpdaterArtifacts: false };

const windows = { ...(config.bundle.windows ?? {}) };
for (const key of ['certificateThumbprint', 'signCommand', 'digestAlgorithm', 'timestampUrl', 'tsp']) {
  delete windows[key];
}
config.bundle.windows = windows;

writeFileSync(configPath, `${JSON.stringify(config, null, 2)}\n`, 'utf8');
console.log(`unsigned CI config written to ${configPath}`);
