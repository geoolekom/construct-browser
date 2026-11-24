import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import net from 'node:net';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { describe, expect, it } from 'vitest';
import {
  agentSocketPath,
  callAgent,
  dashboardLimits,
  readAgentStatus,
  settingsLimits,
} from '../src/agent-socket';

const shortDeadlineMs = 300;

function tempRuntimeDir(): string {
  const runtimeDir = mkdtempSync(path.join(tmpdir(), 'cb-unit-'));
  mkdirSync(path.join(runtimeDir, 'construct'));
  return runtimeDir;
}

// A fake agent: `onConnection` decides what it does with each client.
async function withFakeAgent(
  onConnection: (client: net.Socket) => void,
  check: (env: NodeJS.ProcessEnv) => Promise<void>,
): Promise<void> {
  const runtimeDir = tempRuntimeDir();
  const clients = new Set<net.Socket>();
  const server = net.createServer((client) => {
    clients.add(client);
    onConnection(client);
  });
  await new Promise<void>((resolve) => {
    server.listen(path.join(runtimeDir, 'construct', 'agent.sock'), resolve);
  });
  try {
    await check({ XDG_RUNTIME_DIR: runtimeDir });
  } finally {
    clients.forEach((client) => {
      client.destroy();
    });
    await new Promise((resolve) => server.close(resolve));
    rmSync(runtimeDir, { recursive: true, force: true });
  }
}

describe('agentSocketPath', () => {
  it('follows the platform', () => {
    expect(agentSocketPath({ XDG_RUNTIME_DIR: '/run/user/1000' }, 'linux')).toBe(
      '/run/user/1000/construct/agent.sock',
    );
    expect(agentSocketPath({ TMPDIR: '/var/folders/x/T' }, 'darwin')).toBe(
      '/var/folders/x/T/construct/agent.sock',
    );
  });

  it.each([
    ['linux without a runtime directory', {}, 'linux', /XDG_RUNTIME_DIR is not set/],
    ['linux with a relative runtime directory', { XDG_RUNTIME_DIR: 'run' }, 'linux', /not set/],
    ['macOS without TMPDIR', { XDG_RUNTIME_DIR: '/run/user/1000' }, 'darwin', /TMPDIR is not set/],
    ['Windows', { XDG_RUNTIME_DIR: '/run/user/1000' }, 'win32', /Windows/],
  ] as const)('has no path on %s', (_name, env, platform, message) => {
    expect(() => agentSocketPath(env, platform)).toThrow(message);
  });
});

describe('readAgentStatus', () => {
  it('reports an agent that answers as running', async () => {
    await withFakeAgent(
      (client) => client.end('{"status":"running","version":"0.1.0","pid":7}\n'),
      async (env) => {
        expect(await readAgentStatus(env, 'linux')).toEqual({
          state: 'running',
          version: '0.1.0',
          pid: 7,
          startAtLogin: 'unknown',
        });
      },
    );
  });

  it('carries the service field of the agent', async () => {
    await withFakeAgent(
      (client) =>
        client.end('{"status":"running","version":"0.1.0","pid":7,"service":"registered"}\n'),
      async (env) => {
        expect(await readAgentStatus(env, 'linux')).toEqual({
          state: 'running',
          version: '0.1.0',
          pid: 7,
          startAtLogin: 'registered',
        });
      },
    );
  });

  it('explains that the agent is not supported on Windows', async () => {
    expect(await readAgentStatus({}, 'win32')).toEqual({
      state: 'unknown',
      reason: 'the Construct agent is not supported on Windows yet',
      startAtLogin: 'not_supported',
    });
  });

  it('reports no socket file as stopped', async () => {
    const runtimeDir = tempRuntimeDir();
    try {
      const status = await readAgentStatus({ XDG_RUNTIME_DIR: runtimeDir }, 'linux');
      expect(status.state).toBe('stopped');
    } finally {
      rmSync(runtimeDir, { recursive: true, force: true });
    }
  });

  it('reports a socket file with nobody listening as stopped', async () => {
    // A crashed agent leaves its socket file behind; a plain file at the path refuses the same way.
    const runtimeDir = tempRuntimeDir();
    writeFileSync(path.join(runtimeDir, 'construct', 'agent.sock'), '');
    try {
      const status = await readAgentStatus({ XDG_RUNTIME_DIR: runtimeDir }, 'linux');
      expect(status.state).toBe('stopped');
    } finally {
      rmSync(runtimeDir, { recursive: true, force: true });
    }
  });

  it('reports a missing runtime directory as unknown, not stopped', async () => {
    expect((await readAgentStatus({}, 'linux')).state).toBe('unknown');
  });

  it.each([
    ['a reply that is not JSON', (client: net.Socket) => client.end('hello\n'), /JSON/],
    [
      'a reply without a pid',
      (client: net.Socket) => client.end('{"status":"running","version":"0.1.0"}\n'),
      /no pid/,
    ],
    ['a close without a reply', (client: net.Socket) => client.end(), /without an answer/],
    ['no reply at all', () => undefined, /in time/],
    [
      'a trickle that never ends a line, faster than an idle timeout',
      (client: net.Socket) => {
        const timer = setInterval(() => client.write('x'), 20);
        client.on('close', () => {
          clearInterval(timer);
        });
      },
      /in time/,
    ],
    [
      'an endless reply with no newline',
      (client: net.Socket) => client.write('x'.repeat(100_000)),
      /too long/,
    ],
  ])('reports %s as unknown', async (_name, behave, message) => {
    await withFakeAgent(behave, async (env) => {
      const status = await readAgentStatus(env, 'linux', shortDeadlineMs);
      expect(status).toEqual({
        state: 'unknown',
        reason: expect.stringMatching(message),
        startAtLogin: 'unknown',
      });
    });
  });
});

describe('callAgent with the dashboard limits', () => {
  // One JSON line of about `chars` characters, as the agent answers `dashboard-numbers`.
  const longReply = (chars: number): string =>
    `${JSON.stringify({ groups: [{ key: 'x'.repeat(chars) }] })}\n`;

  it('takes an answer far longer than a status', async () => {
    const reply = longReply(200_000);
    await withFakeAgent(
      (client) => {
        client.end(reply);
      },
      async (env) => {
        const line = await callAgent(env, 'linux', 'dashboard-numbers {}', dashboardLimits);
        expect(line).toBe(reply.trim());
      },
    );
  });

  it('refuses the same answer under the limits of a status or a setting', async () => {
    await withFakeAgent(
      (client) => {
        client.end(longReply(200_000));
      },
      async (env) => {
        await expect(callAgent(env, 'linux', 'settings-get', settingsLimits)).rejects.toThrow(
          /too long/,
        );
      },
    );
  });

  it('refuses an answer past its own limit', async () => {
    await withFakeAgent(
      (client) => {
        client.end(longReply(dashboardLimits.maxReplyChars + 10));
      },
      async (env) => {
        await expect(
          callAgent(env, 'linux', 'dashboard-numbers {}', dashboardLimits),
        ).rejects.toThrow(/too long/);
      },
    );
  });
});
