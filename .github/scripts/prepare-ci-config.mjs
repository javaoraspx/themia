// Produce the Tauri config a CI build uses: the repo's own `tauri.conf.json`
// with exactly the two things a GitHub runner cannot satisfy stripped out.
//
//  - Authenticode signing (`bundle.windows.certificateThumbprint`) needs the
//    author's Certum/SimplySign token plugged into the runner's certificate
//    store. Without it `signtool` fails and the build stops after the (very
//    long) Rust compile.
//  - Updater artifact signing (`bundle.createUpdaterArtifacts`) needs
//    `TAURI_SIGNING_PRIVATE_KEY`, a secret this fork does not have.
//
// The repository's own config is never edited — the release script depends on
// it — and the generated file is written next to it so every relative path in
// the config (`frontendDist`, `icons`, `installerHooks`) still resolves the way
// it does for a normal build. Everything else, NSIS target included, is left
// alone: the artifact is a normal installer that simply isn't code-signed.
import { readFileSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const repoRoot = join(dirname(fileURLToPath(import.meta.url)), '..', '..');
const source = join(repoRoot, 'src-tauri', 'tauri.conf.json');
const target = join(repoRoot, 'src-tauri', 'tauri.build.json');

const config = JSON.parse(readFileSync(source, 'utf8'));

config.bundle = { ...(config.bundle ?? {}), createUpdaterArtifacts: false };

const windows = { ...(config.bundle.windows ?? {}) };
for (const key of ['certificateThumbprint', 'signCommand', 'digestAlgorithm', 'timestampUrl', 'tsp']) {
  delete windows[key];
}
config.bundle.windows = windows;

writeFileSync(target, `${JSON.stringify(config, null, 2)}\n`, 'utf8');
console.log(`wrote ${target}`);
