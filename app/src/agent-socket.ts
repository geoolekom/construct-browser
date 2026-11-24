import net from 'node:net';
import path from 'node:path';
import { parseStatusReply } from './agent-status';
import type { AgentStatus } from './agent-status';

const socketDir = 'construct';
const socketName = 'agent.sock';

/** Where the agent listens: `$XDG_RUNTIME_DIR` on Linux, `$TMPDIR` on macOS. */
export function agentSocketPath(env: NodeJS.ProcessEnv, platform: NodeJS.Platform): string {
  if (platform === 'win32') {
    throw new Error('the Construct agent is not supported on Windows yet');
  }
  const variable = platform === 'darwin' ? 'TMPDIR' : 'XDG_RUNTIME_DIR';
  const dir = env[variable];
  if (dir === undefined || !path.isAbsolute(dir)) {
    throw new Error(`${variable} is not set`);
  }
  return path.join(dir, socketDir, socketName);
}

/** How long one call waits, and how much of a reply it keeps. */
export interface CallLimits {
  readonly deadlineMs: number;
  readonly maxReplyChars: number;
}

export const statusLimits: CallLimits = { deadlineMs: 2000, maxReplyChars: 4096 };
export const settingsLimits: CallLimits = { deadlineMs: 5000, maxReplyChars: 4096 };
/**
 * `dashboard-numbers`: the agent reads the activity store, and the answer lists every group of a
 * table and every day of a period, so it is far longer than a status.
 */
export const dashboardLimits: CallLimits = { deadlineMs: 10_000, maxReplyChars: 512 * 1024 };
/** `dashboard-setup-*`: a status holds the run's report, which is at most 64 KiB of text. */
export const setupLimits: CallLimits = { deadlineMs: 5000, maxReplyChars: 256 * 1024 };

/** How a streamed call is bounded: silence, one line, and everything together. */
export interface StreamLimits {
  readonly idleMs: number;
  readonly maxLineChars: number;
  readonly maxTotalChars: number;
}

/**
 * The agent gives a model 60 s for its first word and 60 s of silence after that; the app waits a
 * little longer, so the agent's own reason wins.
 */
export const chatStreamLimits: StreamLimits = {
  idleMs: 75_000,
  maxLineChars: 64 * 1024,
  maxTotalChars: 2 * 1024 * 1024,
};
/** `chat-get` and the like: the agent answers at once, but it may send many lines. */
export const chatReadLimits: StreamLimits = {
  idleMs: 5000,
  maxLineChars: 512 * 1024,
  maxTotalChars: 16 * 1024 * 1024,
};

/** The agent said nothing for longer than the call allows. */
export class AgentSilentError extends Error {
  constructor() {
    super('the agent went silent');
    this.name = 'AgentSilentError';
  }
}

export interface AgentStream {
  /** Closes the socket. Nothing is reported after it. */
  readonly close: () => void;
}

/**
 * One call whose reply is many lines: each one goes to `onLine` as it arrives. `onEnd` runs once,
 * when the agent closes the socket (`null`) or the call fails: nobody there, too long a line, too
 * much in all, or too long a silence. A last line without its newline is not a line. The
 * request line may hold a message, so no error repeats it.
 */
export function streamAgent(
  socketPath: string,
  line: string,
  limits: StreamLimits,
  onLine: (line: string) => void,
  onEnd: (error: Error | null) => void,
): AgentStream {
  let finished = false;
  let pending = '';
  let total = 0;
  let idle: ReturnType<typeof setTimeout> | undefined;
  const socket = net.createConnection(socketPath);
  const finish = (error: Error | null): void => {
    if (finished) {
      return;
    }
    finished = true;
    clearTimeout(idle);
    socket.destroy();
    onEnd(error);
  };
  const fail = (message: string): void => {
    finish(new Error(message));
  };
  const stillWaiting = (): void => {
    clearTimeout(idle);
    idle = setTimeout(() => {
      finish(new AgentSilentError());
    }, limits.idleMs);
  };
  stillWaiting();
  socket.setEncoding('utf8');
  socket.on('connect', () => {
    socket.write(`${line}\n`);
  });
  socket.on('data', (chunk: string) => {
    stillWaiting();
    total += chunk.length;
    if (total > limits.maxTotalChars) {
      fail('the agent reply is too long');
      return;
    }
    pending += chunk;
    let end = pending.indexOf('\n');
    while (end !== -1 && !finished) {
      const complete = pending.slice(0, end);
      pending = pending.slice(end + 1);
      if (complete.length > limits.maxLineChars) {
        fail('the agent reply is too long');
        return;
      }
      if (complete.trim() !== '') {
        onLine(complete);
      }
      end = pending.indexOf('\n');
    }
    if (!finished && pending.length > limits.maxLineChars) {
      fail('the agent reply is too long');
    }
  });
  socket.on('error', (error) => {
    finish(error);
  });
  socket.on('close', () => {
    finish(null);
  });
  return {
    close: () => {
      if (!finished) {
        finished = true;
        clearTimeout(idle);
        socket.destroy();
      }
    },
  };
}

/** Sends `line` and gives every line the agent answers before it closes the socket. */
export function callAgentLines(
  env: NodeJS.ProcessEnv,
  platform: NodeJS.Platform,
  line: string,
  limits: StreamLimits,
): Promise<string[]> {
  return new Promise((resolve, reject) => {
    const lines: string[] = [];
    streamAgent(
      agentSocketPath(env, platform),
      line,
      limits,
      (reply) => lines.push(reply),
      (error) => {
        if (error === null) {
          resolve(lines);
        } else {
          reject(error);
        }
      },
    );
  });
}

/**
 * Sends `line` to the agent and gives its one reply line; fails when nobody answers. The deadline
 * is for the whole call, whatever the agent writes meanwhile, and the reply is capped, so a wedged
 * or chatty agent cannot keep the call or its memory. The line may hold an API key, so no error
 * repeats it.
 */
export function callAgent(
  env: NodeJS.ProcessEnv,
  platform: NodeJS.Platform,
  line: string,
  limits: CallLimits,
): Promise<string> {
  const socketPath = agentSocketPath(env, platform);
  return new Promise((resolve, reject) => {
    let settled = false;
    const settle = (finish: () => void): void => {
      if (!settled) {
        settled = true;
        clearTimeout(deadline);
        finish();
      }
    };
    const stream = streamAgent(
      socketPath,
      line,
      // Silence is bounded by the deadline below; the stream's own limit only has to be later.
      {
        idleMs: limits.deadlineMs * 2,
        maxLineChars: limits.maxReplyChars,
        maxTotalChars: limits.maxReplyChars * 2,
      },
      (reply) => {
        settle(() => {
          stream.close();
          resolve(reply.trim());
        });
      },
      (error) => {
        settle(() => {
          reject(error ?? new Error('the agent closed the socket without an answer'));
        });
      },
    );
    const deadline = setTimeout(() => {
      settle(() => {
        stream.close();
        reject(new Error('the agent did not answer in time'));
      });
    }, limits.deadlineMs);
  });
}

export interface Where {
  readonly env: NodeJS.ProcessEnv;
  readonly platform: NodeJS.Platform;
}

export function isAgentAbsent(error: unknown): boolean {
  return (
    error instanceof Error &&
    'code' in error &&
    (error.code === 'ENOENT' || error.code === 'ECONNREFUSED')
  );
}

/**
 * Asks the agent for its status. `stopped` means the socket says nobody is there; any other
 * failure (no runtime directory, a bad or late reply) is `unknown`, not a claim about the agent.
 */
export async function readAgentStatus(
  env: NodeJS.ProcessEnv,
  platform: NodeJS.Platform,
  deadlineMs = statusLimits.deadlineMs,
): Promise<AgentStatus> {
  try {
    const line = await callAgent(env, platform, 'status', { ...statusLimits, deadlineMs });
    return parseStatusReply(line);
  } catch (error) {
    const reason = error instanceof Error ? error.message : String(error);
    return {
      state: isAgentAbsent(error) ? 'stopped' : 'unknown',
      reason,
      // Windows has no agent socket yet and the app cannot register the agent there.
      startAtLogin: platform === 'win32' ? 'not_supported' : 'unknown',
    };
  }
}

/**
 * A failure as text for the window. The messages here come from the socket and from the agent's
 * fixed texts; none of them holds a line that was sent or an answer that came back, because those
 * carry the API key and the user's messages.
 */
export function failureText(error: unknown): string {
  if (isAgentAbsent(error)) {
    return 'the agent is not running';
  }
  // A JSON syntax error quotes the text it could not read, which is the agent's answer.
  if (error instanceof SyntaxError) {
    return 'the agent gave an answer the app cannot read';
  }
  return error instanceof Error ? error.message : 'the agent call failed';
}
