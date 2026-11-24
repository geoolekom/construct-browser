import { execFile } from 'node:child_process';
import path from 'node:path';

export const agentName = process.platform === 'win32' ? 'construct-agent.exe' : 'construct-agent';

/** Set to `1` to register the agent from a development run too (see the README). */
export const REGISTER_IN_DEV_VARIABLE = 'CONSTRUCT_AGENT_REGISTER';

export type AgentLocation = {
  readonly packaged: boolean;
  /** `process.resourcesPath`: where Electron Forge's `extraResource` puts the agent. */
  readonly resourcesPath: string;
  /** `app.getAppPath()`: the `app/` directory in a development run. */
  readonly appPath: string;
};

/** The agent that ships with the app, or the `cargo` output in a development run. */
export function agentBinaryPath(location: AgentLocation): string {
  if (location.packaged) {
    return path.join(location.resourcesPath, agentName);
  }
  return path.join(location.appPath, '..', 'target', 'release', agentName);
}

/**
 * A development build would leave a service that points into the build tree, so it
 * registers only when asked to; an installed app always does.
 */
export function shouldRegisterAgent(packaged: boolean, env: NodeJS.ProcessEnv): boolean {
  return packaged || env[REGISTER_IN_DEV_VARIABLE] === '1';
}

/** An install on a second disk, `/Volumes/<disk>/Applications/…`, as opposed to a disk image. */
export function isInstalledDiskPath(binary: string): boolean {
  return /^\/Volumes\/[^/]+\/Applications\//.test(binary);
}

/**
 * Where macOS runs an app from only for a while: a read-only App Translocation copy (deleted
 * at logout or restart) or a mounted disk image. A login service that points there is left
 * with a missing file.
 */
export function isTransientAppPath(binary: string, platform: NodeJS.Platform): boolean {
  if (platform !== 'darwin') {
    return false;
  }
  // A second disk can hold a real install (`/Volumes/Data/Applications/…`); a disk image
  // holds the app at its top or in a folder of its own.
  return (
    binary.includes('/AppTranslocation/') ||
    (binary.startsWith('/Volumes/') && !isInstalledDiskPath(binary))
  );
}

export type RegistrationDecision = 'register' | 'skip-development' | 'skip-transient-path';

/** Whether the app registers the agent as a login service, from where it runs now. */
export function registrationDecision(
  packaged: boolean,
  env: NodeJS.ProcessEnv,
  binary: string,
  platform: NodeJS.Platform,
): RegistrationDecision {
  if (!shouldRegisterAgent(packaged, env)) {
    return 'skip-development';
  }
  return isTransientAppPath(binary, platform) ? 'skip-transient-path' : 'register';
}

/**
 * Asks the agent to register itself as a login service. `ensure-service` does nothing when
 * the service is registered for this very binary, so this is safe at every start.
 */
export function ensureAgentService(binary: string, env: NodeJS.ProcessEnv): Promise<string> {
  return new Promise((resolve, reject) => {
    execFile(binary, ['ensure-service'], { env, timeout: 30_000 }, (error, stdout, stderr) => {
      if (error) {
        reject(new Error(`ensure-service failed: ${stderr.trim() || error.message}`));
      } else {
        resolve(stdout.trim());
      }
    });
  });
}
