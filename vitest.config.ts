import { defineConfig } from 'vitest/config';
import solid from 'vite-plugin-solid';

export default defineConfig({
  // Solid's JSX transform, so a test can render a real widget component. This
  // file is the whole Vite config for the unit suite (Vitest does not merge
  // vite.config.ts), and without the plugin a `.tsx` import arrives with its
  // JSX intact — which the runner rejects as "invalid JS syntax" rather than
  // telling you a plugin is missing. Only component tests pull in `.tsx`; the
  // pure-logic suites are unaffected, and the explicit `solid-js/*` aliases
  // below still win over the conditions this plugin adds.
  //
  // `hot: false` is required: the default injects solid-refresh for HMR, whose
  // virtual module (`file:///@solid-refresh`) has no path a test runner can
  // resolve, so the suite dies while being collected.
  plugins: [solid({ hot: false })],
  // Mirror the vite.config build-time constant so modules that reference it
  // (e.g. license.ts gating its test hooks) load under vitest instead of
  // throwing `__THEMIA_TEST_HOOKS__ is not defined`. Tests run with hooks on.
  define: {
    __THEMIA_TEST_HOOKS__: JSON.stringify(true),
  },
  // Pin Solid's PRODUCTION client builds (not the SSR `server` build the default
  // `node` condition selects, nor the `dev` build Vitest's auto-added
  // `development` condition selects). Tests that mount a reactive root via
  // `solid-js/web` `render` (e.g. createCachedFetch.test.ts) need the client
  // runtime so effects/memos actually flush; the SSR build throws "Client-only
  // API called on the server side", and the dev build needs a dev global the
  // node-env unit tests don't initialise. The production client builds are a
  // single consistent runtime that all tests (pure-logic + render) share.
  resolve: {
    alias: [
      { find: /^solid-js\/web$/, replacement: 'solid-js/web/dist/web.js' },
      { find: /^solid-js\/store$/, replacement: 'solid-js/store/dist/store.js' },
      { find: /^solid-js$/, replacement: 'solid-js/dist/solid.js' },
    ],
  },
  test: {
    // Default node env (fast). Tests that need DOM/localStorage opt in per-file
    // with `// @vitest-environment jsdom`. The setup file stubs BroadcastChannel
    // so store modules import cleanly in both envs.
    environment: 'node',
    setupFiles: ['./vitest.setup.ts'],
    include: ['src/**/*.test.ts', 'widgets/**/*.test.ts'],
    // `*.network.test.ts` suites contact real third-party servers, so they are
    // opt-in (`npm run test:presets`, vitest.presets.config.ts) and never part
    // of the offline `npm run check` gate. [S-48]
    exclude: ['**/node_modules/**', '**/dist/**', '**/*.network.test.ts'],
  },
});
