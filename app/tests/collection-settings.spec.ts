import { execFileSync } from 'node:child_process';
import { existsSync, mkdirSync, readdirSync, readFileSync, statSync, writeFileSync } from 'node:fs';
import path from 'node:path';
import type { Locator, Page } from '@playwright/test';
import { isRecord } from '../src/agent-json';
import {
  agentBinary,
  checkInApp,
  claudeFile,
  claudePrompt,
  expect,
  openSettings,
  restartAgent,
  test as base,
  writeLine,
} from './fixtures';
import type { Session } from './fixtures';

// The agent looks at the session files every 5 seconds. A wait is for several passes, so a slow
// runner does not turn it red; a wait that ends at once when the agent has done what it should.
const waitMs = 60_000;

/**
 * Waits until the agent's Construct host is ready. The agent turns collection on only once it can
 * ask the host, and refuses until then, so a switch pressed right after a start would be refused.
 */
async function expectHostReady(session: Session): Promise<void> {
  await expect
    .poll(
      () => {
        const status: unknown = JSON.parse(
          execFileSync(agentBinary, ['status'], { env: session.env, encoding: 'utf8' }),
        );
        if (!isRecord(status)) {
          return undefined;
        }
        const host = status.construct;
        return isRecord(host) ? host.state : host;
      },
      { timeout: waitMs },
    )
    .toBe('ready');
}

const codexSessionId = '01a0bee2-6260-73a3-b389-43c86bb8930f';

const stamp = (): string => new Date().toISOString();

function codexFile(session: Session): string {
  return path.join(
    session.codexHome,
    'sessions',
    '2026',
    '11',
    '13',
    `rollout-2026-11-13T10-00-00-${codexSessionId}.jsonl`,
  );
}

/** The text of a stored record when it is a turn; anything else the agent writes gives `[]`. */
function turnText(line: string): string[] {
  const record: unknown = JSON.parse(line);
  if (!isRecord(record) || record.kind !== 'turn') {
    return [];
  }
  if (typeof record.text !== 'string') {
    throw new Error(`a stored turn has no text: ${line}`);
  }
  return [record.text];
}

/** The texts of the turns the agent has stored for a tool. */
function stored(session: Session, tool: 'codex' | 'claude-code'): string[] {
  const dir = path.join(session.recordsDir, tool);
  if (!existsSync(dir)) {
    return [];
  }
  return readdirSync(dir)
    .flatMap((name) => readFileSync(path.join(dir, name), 'utf8').split('\n'))
    .filter((line) => line !== '')
    .flatMap(turnText);
}

/**
 * How far the agent has read `file`, from its saved progress. The agent's `status` says only
 * that a pass has seen the tool off, at the start of the pass; the saved position is the one
 * thing that says a line was read and skipped, so it is read here, and a changed format fails
 * loudly instead of timing out.
 */
function readUpTo(session: Session, file: string): number | undefined {
  const progress = path.join(session.recordsDir, '..', 'connectors-progress.json');
  if (!existsSync(progress)) {
    return undefined;
  }
  const saved: unknown = JSON.parse(readFileSync(progress, 'utf8'));
  if (!isRecord(saved) || !isRecord(saved.files)) {
    throw new Error('the agent progress file has no `files`');
  }
  const entry = Object.entries(saved.files).find(([key]) => key.endsWith(path.basename(file)));
  if (entry === undefined) {
    return undefined;
  }
  const [, position] = entry;
  if (!isRecord(position) || typeof position.offset !== 'number') {
    throw new Error('the agent progress file has an entry with no `offset`');
  }
  return position.offset;
}

/**
 * Waits until the agent has read everything in `file` as it is now. With the tool off that is
 * how a line written meanwhile is known to be skipped, not merely not stored yet.
 */
async function expectRead(session: Session, file: string): Promise<void> {
  const { size } = statSync(file);
  await expect.poll(() => readUpTo(session, file), { timeout: waitMs }).toBe(size);
}

interface CodexSession {
  readonly file: string;
  readonly prompt: (text: string) => void;
}

const test = base.extend<{ codexSession: CodexSession; collectionPage: Page }>({
  // A Codex session file with its first line, which a reader needs to know the session.
  codexSession: async ({ session }, use) => {
    const file = codexFile(session);
    writeLine(file, {
      timestamp: stamp(),
      type: 'session_meta',
      payload: { id: codexSessionId, cwd: '/w' },
    });
    await use({
      file,
      prompt: (text) => {
        writeLine(file, {
          timestamp: stamp(),
          type: 'response_item',
          payload: { type: 'message', role: 'user', content: [{ type: 'input_text', text }] },
        });
      },
    });
  },
  collectionPage: async ({ page, session }, use) => {
    await expectHostReady(session);
    await openSettings(page, 'Collection');
    await use(page);
  },
});

const master = (page: Page): Locator =>
  page.getByRole('switch', { name: 'Collect from my coding tools' });
const claude = (page: Page): Locator => page.getByRole('switch', { name: 'Claude Code' });
const codex = (page: Page): Locator => page.getByRole('switch', { name: 'Codex' });

test('the three switches start on', async ({ collectionPage: page }) => {
  for (const toggle of [master(page), claude(page), codex(page)]) {
    await expect(toggle).toBeChecked();
    await expect(toggle).toBeEnabled();
  }
});

test('the switches can be reached and operated by keyboard', async ({ collectionPage: page }) => {
  await master(page).focus();
  await expect(master(page)).toBeFocused();
  await page.keyboard.press('Space');
  await expect(master(page)).not.toBeChecked();
  await expect(codex(page)).toBeDisabled();
  await page.keyboard.press('Space');
  await expect(master(page)).toBeChecked();

  await page.keyboard.press('Tab');
  await expect(claude(page)).toBeFocused();
  await page.keyboard.press('Tab');
  await expect(codex(page)).toBeFocused();
  await page.keyboard.press('Space');
  await expect(codex(page)).not.toBeChecked();
  await expect(claude(page)).toBeChecked();
});

test('Codex off collects nothing written meanwhile, and Codex on collects what comes after', async ({
  session,
  codexSession,
  collectionPage: page,
}) => {
  test.setTimeout(3 * waitMs);

  await codex(page).click();
  await expect(codex(page)).not.toBeChecked();
  codexSession.prompt('typed while off');
  await expectRead(session, codexSession.file);
  expect(stored(session, 'codex')).toEqual([]);

  await codex(page).click();
  await expect(codex(page)).toBeChecked();
  codexSession.prompt('typed after on');
  // The prompt written while off was read and skipped, so only this one is stored.
  await expect
    .poll(() => stored(session, 'codex'), { timeout: waitMs })
    .toEqual(['typed after on']);
});

test('Claude Code off collects nothing written meanwhile, and on collects what comes after', async ({
  session,
  collectionPage: page,
}) => {
  test.setTimeout(3 * waitMs);

  await claude(page).click();
  await expect(claude(page)).not.toBeChecked();
  claudePrompt(session, 'typed while off');
  await expectRead(session, claudeFile(session));
  expect(stored(session, 'claude-code')).toEqual([]);

  await claude(page).click();
  await expect(claude(page)).toBeChecked();
  claudePrompt(session, 'typed after on');
  await expect
    .poll(() => stored(session, 'claude-code'), { timeout: waitMs })
    .toEqual(['typed after on']);
});

test('the master switch turns both tools off and keeps what each tool had', async ({
  session,
  codexSession,
  collectionPage: page,
}) => {
  test.setTimeout(4 * waitMs);
  // Codex has its own switch off before the master goes off.
  await codex(page).click();
  await expect(codex(page)).not.toBeChecked();

  await master(page).click();
  await expect(master(page)).not.toBeChecked();
  // Under an off master both show off and cannot be changed.
  for (const toggle of [claude(page), codex(page)]) {
    await expect(toggle).not.toBeChecked();
    await expect(toggle).toBeDisabled();
  }
  codexSession.prompt('codex while all off');
  claudePrompt(session, 'claude while all off');
  await expectRead(session, codexSession.file);
  await expectRead(session, claudeFile(session));
  expect(stored(session, 'codex')).toEqual([]);
  expect(stored(session, 'claude-code')).toEqual([]);

  // The master on restores each tool's own value: Claude Code on, Codex still off.
  await master(page).click();
  await expect(master(page)).toBeChecked();
  await expect(claude(page)).toBeChecked();
  await expect(claude(page)).toBeEnabled();
  await expect(codex(page)).not.toBeChecked();
  claudePrompt(session, 'claude after on');
  codexSession.prompt('codex still off');
  await expect
    .poll(() => stored(session, 'claude-code'), { timeout: waitMs })
    .toEqual(['claude after on']);
  await expectRead(session, codexSession.file);
  expect(stored(session, 'codex')).toEqual([]);
});

test('the switches keep their state when the app and the agent are restarted', async ({
  session,
}) => {
  await checkInApp(session, async (page) => {
    await openSettings(page, 'Collection');
    await codex(page).click();
    await expect(codex(page)).not.toBeChecked();
  });
  await restartAgent(session);

  await checkInApp(session, async (page) => {
    await openSettings(page, 'Collection');
    await expect(master(page)).toBeChecked();
    await expect(claude(page)).toBeChecked();
    await expect(codex(page)).not.toBeChecked();

    await master(page).click();
    await expect(master(page)).not.toBeChecked();
  });
  await restartAgent(session);

  await checkInApp(session, async (page) => {
    await openSettings(page, 'Collection');
    await expect(master(page)).not.toBeChecked();
    await expect(claude(page)).toBeDisabled();
  });
});

test('an agent that started with collection off says why, and the master switch answers it', async ({
  session,
}) => {
  await session.stopAgent();
  const dir = path.join(session.configHome, 'construct');
  mkdirSync(dir, { recursive: true });
  writeFileSync(path.join(dir, 'settings.json'), '{ not json');
  await session.startAgent();
  await expectHostReady(session);

  await checkInApp(session, async (page) => {
    await openSettings(page, 'Collection');
    await expect(page.locator('#collection-notice')).toContainText('unreadable');
    await expect(master(page)).not.toBeChecked();

    await master(page).click();
    await expect(master(page)).toBeChecked();
    await expect(page.locator('#collection-notice')).toHaveCount(0);
  });
});

test('a stopped agent shows its error, and the next save that works clears it', async ({
  session,
  collectionPage: page,
}) => {
  await session.stopAgent();

  await codex(page).click();

  await expect(page.locator('#collection-error')).toContainText('Not done');
  // The switch did not move: it shows what the agent last reported.
  await expect(codex(page)).toBeChecked();

  await session.startAgent();
  await codex(page).click();

  await expect(codex(page)).not.toBeChecked();
  await expect(page.locator('#collection-error')).toHaveCount(0);
});
