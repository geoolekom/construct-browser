import { FuseV1Options, FuseVersion } from '@electron/fuses';
import { MakerDeb } from '@electron-forge/maker-deb';
import { MakerDMG } from '@electron-forge/maker-dmg';
import { MakerSquirrel } from '@electron-forge/maker-squirrel';
import { FusesPlugin } from '@electron-forge/plugin-fuses';
import { VitePlugin } from '@electron-forge/plugin-vite';
import type { ForgeConfig } from '@electron-forge/shared-types';
import { macInfoPlist } from './mac-bundle.mjs';
import { agentName } from './src/agent-service';

const config: ForgeConfig = {
  packagerConfig: {
    asar: true,
    appBundleId: 'cf.construct.browser',
    osxSign: {
      identity: '-',
      identityValidation: false,
      optionsForFile: () => ({ hardenedRuntime: false }),
    },
    // The name of the program in the package: /usr/bin/construct-browser, not the product name with a space.
    executableName: 'construct-browser',
    // Only macOS reads these keys; the other systems ignore them.
    extendInfo: macInfoPlist,
    // The agent is its own program: it goes next to the app, outside the asar archive,
    // and the app finds it in `process.resourcesPath`.
    extraResource: [`../target/release/${agentName}`],
  },
  rebuildConfig: {},
  makers: [
    new MakerSquirrel({
      authors: 'Constructor Fabric Foundation',
      setupExe: 'construct-browser-x64-Setup.exe',
    }),
    new MakerDMG({}),
    new MakerDeb({ options: { name: 'construct-browser', bin: 'construct-browser' } }),
  ],
  plugins: [
    new VitePlugin({
      build: [
        {
          entry: 'src/main.ts',
          config: 'vite.main.config.mts',
          target: 'main',
        },
        {
          entry: 'src/preload.ts',
          config: 'vite.preload.config.mts',
          target: 'preload',
        },
      ],
      renderer: [
        {
          name: 'main_window',
          config: 'vite.renderer.config.mts',
        },
      ],
    }),
    // Fuses are used to enable/disable various Electron functionality
    // at package time, before code signing the application
    new FusesPlugin({
      version: FuseVersion.V1,
      [FuseV1Options.RunAsNode]: false,
      [FuseV1Options.EnableCookieEncryption]: true,
      [FuseV1Options.EnableNodeOptionsEnvironmentVariable]: false,
      [FuseV1Options.EnableNodeCliInspectArguments]: false,
      [FuseV1Options.EnableEmbeddedAsarIntegrityValidation]: true,
      [FuseV1Options.OnlyLoadAppFromAsar]: true,
    }),
  ],
};

export default config;
