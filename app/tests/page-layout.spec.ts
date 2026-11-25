import type { Locator, Page } from '@playwright/test';
import { configure, input, test } from './chat-helpers';
import { expect, withStandIn } from './fixtures';
import type { OpenedApp, Session } from './fixtures';
import { gridPlaces, now, settingsReply, widget } from './dashboard-shared';
import { newHost, openMemory, withHostAndAgent } from './memory-host';
import type { Fact } from './memory-host';
import { screenshotBothThemes } from './screenshots';

const sizes = [
  { name: 'smallest', width: 720, height: 480 },
  { name: 'initial', width: 960, height: 640 },
  { name: 'full-screen', width: 1920, height: 1080 },
] as const;

/**
 * Gives the window this content size and waits until the page reports the width. The height is
 * not checked: a screen shorter than the window keeps it shorter.
 */
async function resize(
  app: OpenedApp['app'],
  page: Page,
  width: number,
  height: number,
): Promise<void> {
  await app.evaluate(
    ({ BrowserWindow }, size) => {
      BrowserWindow.getAllWindows()[0]?.setContentSize(size.width, size.height);
    },
    { width, height },
  );
  await expect.poll(() => page.evaluate(() => window.innerWidth)).toBe(width);
}

/** How far a box starts from the left and top of the page's main area, in pixels. */
async function offset(page: Page, target: Locator): Promise<{ left: number; top: number }> {
  const box = await target.boundingBox();
  const main = await page.locator('main').boundingBox();
  if (box === null || main === null) {
    throw new Error('the box is not on the page');
  }
  return { left: box.x - main.x, top: box.y - main.y };
}

/** The switch with this name. */
const toggle = (page: Page, name: string) => page.getByRole('switch', { name });

const openPage = (page: Page, name: string) =>
  page.getByRole('button', { name, exact: true }).click();

test('every page keeps its content the same distance from the edge of the window', async ({
  app,
  page,
  provider,
}) => {
  await configure(page, { address: provider.url });
  await resize(app, page, 720, 480);

  const starts: Record<string, { left: number; top?: number }> = {};
  for (const name of ['Dashboard', 'Memory', 'Status', 'Settings']) {
    await openPage(page, name);
    const heading = page.locator('main h1');
    await expect(heading).toBeVisible();
    starts[name] = await offset(page, heading);
  }
  await openPage(page, 'Chat');
  await expect(input(page)).toBeVisible();
  // The chat keeps its own scrolling area, so only its sides are compared.
  starts.Chat = { left: (await offset(page, input(page).locator('xpath=..'))).left };

  const left = starts.Settings?.left ?? Number.NaN;
  const top = starts.Settings?.top ?? Number.NaN;
  expect(left).toBeGreaterThan(0);
  for (const name of Object.keys(starts)) {
    expect(starts[name]?.left, `${name} left`).toBeCloseTo(left, 1);
    if (name !== 'Chat') {
      expect(starts[name]?.top, `${name} top`).toBeCloseTo(top, 1);
    }
  }
});

test('the fields, buttons and switches of Settings share their edges and widths', async ({
  app,
  page,
  provider,
}) => {
  await configure(page, { address: provider.url });
  await openPage(page, 'Settings');
  await expect(toggle(page, 'Collect from my coding tools')).toBeVisible();

  for (const size of sizes) {
    await resize(app, page, size.width, size.height);
    await expect(page.locator('#model-base-url')).toBeVisible();
    const box = async (locator: Locator) => {
      const found = await locator.boundingBox();
      if (found === null) {
        throw new Error('the box is not on the page');
      }
      return found;
    };
    const fields = [
      await box(page.locator('#model-base-url')),
      await box(page.locator('#model-name')),
      await box(page.locator('#model-api-key')),
    ];
    for (const field of fields) {
      expect(field.x, `${size.name} field left`).toBeCloseTo(fields[0]?.x ?? 0, 1);
      expect(field.width, `${size.name} field width`).toBeCloseTo(fields[0]?.width ?? 0, 1);
    }
    const save = await box(page.locator('#model-save'));
    expect(save.x, `${size.name} button left`).toBeCloseTo(fields[0]?.x ?? 0, 1);

    const switches = [
      await box(toggle(page, 'Collect from my coding tools')),
      await box(toggle(page, 'Claude Code')),
      await box(toggle(page, 'Codex')),
    ];
    const fieldRight = (fields[0]?.x ?? 0) + (fields[0]?.width ?? 0);
    for (const one of switches) {
      expect(one.x + one.width, `${size.name} switch right`).toBeCloseTo(fieldRight, 1);
      expect(one.width, `${size.name} switch width`).toBeCloseTo(switches[0]?.width ?? 0, 1);
    }
    // The buttons of "Your data" end where the switches do.
    const erase = await box(page.locator('#erase-everything'));
    expect(erase.x + erase.width, `${size.name} button right`).toBeCloseTo(fieldRight, 1);

    await screenshotBothThemes(page, `settings-${size.name}`);
  }
});

/**
 * The space between each block of the open page and the one above it, in pixels. The blocks are
 * the children of the page's column that take room.
 */
async function gaps(page: Page): Promise<number[]> {
  return page.evaluate(() => {
    const column = document.querySelector('main > div:last-child');
    if (column === null) {
      throw new Error('the page has no column');
    }
    const boxes = [...column.children]
      .map((child) => child.getBoundingClientRect())
      .filter((box) => box.height > 0);
    return boxes.slice(1).map((box, index) => box.top - (boxes[index]?.bottom ?? 0));
  });
}

/** How long a page may take to show what the agent last said, or that it is gone. */
const refreshDeadlineMs = 10_000;

/** The space of a page's column between blocks, in pixels: `--space-4` and `--space-8`. */
const normalGap = 16;
const wideGap = 32;
/** The space between the groups of the memory tree, in pixels: `--space-6`. */
const groupGap = 24;

/** The page has exactly `blocks` blocks, and every neighbouring pair is `gap` pixels apart. */
async function expectBlocksApart(
  page: Page,
  blocks: number,
  gap: number,
  what: string,
): Promise<void> {
  await expect.poll(async () => (await gaps(page)).length + 1, { message: what }).toBe(blocks);
  expect(await gaps(page), `${what}: the space between each two blocks`).toEqual(
    Array.from({ length: blocks - 1 }, () => gap),
  );
}

/** The stand-in agent of the Memory page: its settings, with collection on. */
const memoryAgent = (line: string): string =>
  line.startsWith('settings-get')
    ? settingsReply(true)
    : JSON.stringify({ status: 'running', version: '0.1.0', pid: 42 });

const someFacts: Fact[] = [
  { entity_kind: 'identity', property: 'full_name', value: 'Ada Lovelace' },
  { entity_kind: 'role', property: 'job_title', value: 'Engineer' },
  { entity_kind: 'skill', property: 'skills', value: { name: 'Rust' } },
];

async function checkMemory(session: Session, facts: Fact[], shot: string): Promise<void> {
  const host = newHost(session);
  host.facts = facts;
  await withHostAndAgent(session, host, memoryAgent, async (page) => {
    host.announce();
    await openMemory(page).click();
    await expect(page.locator('#memory-collection')).toBeVisible();
    await expect(page.locator(facts.length === 0 ? '#memory-empty' : '#memory-tree')).toBeVisible();
    await expectBlocksApart(page, 3, normalGap, 'Memory');
    const groups = await page
      .locator('#memory-tree section')
      .evaluateAll((kinds) => kinds.map((kind) => kind.getBoundingClientRect()));
    expect(groups, 'the groups of the tree').toHaveLength(facts.length);
    expect(
      groups.slice(1).map((group, index) => group.top - (groups[index]?.bottom ?? 0)),
      'the space between two groups',
    ).toEqual(groups.slice(1).map(() => groupGap));
    await screenshotBothThemes(page, shot);
  });
}

test('on the Memory page the collection line and the empty box are apart', async ({ session }) => {
  await checkMemory(session, [], 'memory-empty-spacing');
});

test('on the Memory page the collection line and a list are apart', async ({ session }) => {
  await checkMemory(session, someFacts, 'memory-list-spacing');
});

const days = (label: string) => ({ label, minutes: 30 });

/** Answers the Dashboard page with a document, a problem with the file, and a hidden project. */
function dashboardAgent(widgets: ReturnType<typeof widget>[], problem: boolean) {
  return (line: string): string => {
    if (line.startsWith('dashboard-numbers ')) {
      const asked = JSON.parse(line.slice('dashboard-numbers '.length)) as {
        days: { label: string }[];
      };
      return JSON.stringify({
        total_minutes: asked.days.length * 30,
        session_count: asked.days.length,
        hidden_project_count: 1,
        groups: [{ key: 'billing', minutes: 30, session_count: 1, share: 1 }],
        days: asked.days.map((day) => days(day.label)),
      });
    }
    if (line === 'dashboard-get') {
      return JSON.stringify({
        document: gridPlaces(widgets),
        ...(problem
          ? {
              error: {
                kind: 'line',
                place: 'line 1, column 1',
                message: 'the file is not valid JSON',
              },
            }
          : {}),
      });
    }
    if (line === 'settings-get') {
      return settingsReply(true);
    }
    return JSON.stringify({ status: 'running', version: '0.1.0', pid: 42 });
  };
}

function dashboardWidgets() {
  return [widget('total', 'total', [0, 0, 12, 2]), widget('top', 'table', [0, 2, 12, 4])];
}

/**
 * Each state of the Dashboard: what shows that the page has loaded it, the blocks it then has
 * (heading, toolbar, the notices, then the rest) and whether it is the one that is photographed.
 */
const dashboardStates = [
  { name: 'empty', widgets: [], problem: false, loaded: ['#dashboard-no-widgets'], blocks: 3 },
  {
    name: 'empty with a problem',
    widgets: [],
    problem: true,
    loaded: ['#dashboard-no-widgets', '#dashboard-problem'],
    blocks: 4,
  },
  {
    name: 'filled',
    widgets: dashboardWidgets(),
    problem: false,
    loaded: ['#widget-total', '#dashboard-hidden-note'],
    blocks: 4,
  },
  {
    name: 'filled with a problem',
    widgets: dashboardWidgets(),
    problem: true,
    loaded: ['#widget-total', '#dashboard-hidden-note', '#dashboard-problem'],
    blocks: 5,
    shot: 'dashboard-notice-spacing',
  },
] as const;

for (const state of dashboardStates) {
  test(`on the Dashboard the notices and the widgets are apart: ${state.name}`, async ({
    session,
  }) => {
    await withStandIn(session, dashboardAgent([...state.widgets], state.problem), async (page) => {
      await page.clock.install({ time: now });
      await page.reload();
      await page.getByRole('button', { name: 'Dashboard', exact: true }).click();
      for (const selector of state.loaded) {
        await expect(page.locator(selector)).toBeVisible();
      }
      await expectBlocksApart(page, state.blocks, normalGap, `Dashboard ${state.name}`);
      if ('shot' in state) {
        await screenshotBothThemes(page, state.shot);
      }
    });
  });
}

// The Status page has 10 blocks here, one card for each source the agent reads (Claude Code,
// Codex, ChatGPT, the Claude app): a platform with a screen to collect adds a card.
test('the Status page keeps its blocks apart with a running agent', async ({ page }) => {
  await openPage(page, 'Status');
  await expect(page.locator('#agent-status')).toHaveText('running');
  await expectBlocksApart(page, 10, normalGap, 'Status running');
});

test('the Settings page keeps its blocks wide apart with a running agent', async ({ page }) => {
  await openPage(page, 'Settings');
  await expect(toggle(page, 'Collect from my coding tools')).toBeVisible();
  await expect(page.getByRole('heading', { name: 'Pair a phone' })).toBeVisible();
  await expectBlocksApart(page, 6, wideGap, 'Settings');
});

test('the Status page keeps its blocks apart without an agent', async ({ page, session }) => {
  session.agent.kill('SIGTERM');
  await openPage(page, 'Status');
  await expect(page.locator('#agent-status')).toHaveText('stopped', { timeout: refreshDeadlineMs });
  await expectBlocksApart(page, 3, normalGap, 'Status stopped');
});

test('the Settings page keeps its blocks wide apart without an agent', async ({
  page,
  session,
}) => {
  // Settings reads once when it opens, so the agent must be gone first, not still shutting down.
  session.agent.kill('SIGTERM');
  if (session.agent.exitCode === null && session.agent.signalCode === null) {
    await new Promise((resolve) => session.agent.once('exit', resolve));
  }
  await openPage(page, 'Settings');
  await expect(page.getByRole('alert').first()).toBeVisible({ timeout: refreshDeadlineMs });
  await expectBlocksApart(page, 2, wideGap, 'Settings without an agent');
});

/**
 * Chat has no column: its thread fills the window and the composer sits under it, so no gap of
 * the page's column applies. What is spaced is the notices of the composer's area.
 */
async function expectChatComposerSpaced(page: Page, what: string): Promise<void> {
  const spacing = await page.evaluate(() => {
    const thread = document.querySelector('#chat-thread')?.closest('main > *');
    const area = document.querySelector('#chat-no-model')?.parentElement;
    if (!thread || !area) {
      throw new Error('the chat page is not shown');
    }
    const boxes = [...area.children]
      .map((child) => child.getBoundingClientRect())
      .filter((box) => box.height > 0);
    return {
      rowGap: getComputedStyle(thread).rowGap,
      gaps: boxes.slice(1).map((box, index) => box.top - (boxes[index]?.bottom ?? 0)),
    };
  });
  // The chat sits straight in the frame, with no column around it.
  expect(spacing.rowGap, `${what}: the chat is in no column`).toBe('normal');
  // The model notice and the composer, with the profile note above them while the gear is not
  // ready: whichever of these the page shows, each is --space-2 from the next.
  expect(spacing.gaps.length, `${what}: the notices and the composer`).toBeGreaterThan(0);
  expect(spacing.gaps, `${what}: the space between the notices and the composer`).toEqual(
    spacing.gaps.map(() => 8),
  );
}

test('on Chat the notice and the composer are apart and the chat is in no column', async ({
  page,
}) => {
  await expect(page.locator('#chat-empty')).toBeVisible();
  await expect(page.locator('#chat-no-model')).toBeVisible({ timeout: refreshDeadlineMs });
  await expectChatComposerSpaced(page, 'Chat with no model');
});
