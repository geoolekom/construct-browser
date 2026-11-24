import path from 'node:path';
import { describe, expect, it } from 'vitest';
import { agentBinaryPath, isTransientAppPath, registrationDecision } from '../src/agent-service';

describe('agentBinaryPath', () => {
  it.each([
    ['packaged', true, ['resources']],
    ['development', false, ['target', 'release']],
  ] as const)('uses the host executable name in a %s app', (_mode, packaged, directory) => {
    const root = path.parse(process.cwd()).root;
    const location = {
      packaged,
      resourcesPath: path.join(root, 'resources'),
      appPath: path.join(root, 'app'),
    };
    const expectedName = process.platform === 'win32' ? 'construct-agent.exe' : 'construct-agent';
    expect(agentBinaryPath(location)).toBe(path.join(root, ...directory, expectedName));
  });
});

const translocated =
  '/private/var/folders/ab/xyz/T/AppTranslocation/1F2E-uuid/d/Construct Browser.app/Contents/Resources/construct-agent';
const mounted =
  '/Volumes/Construct Browser/Construct Browser.app/Contents/Resources/construct-agent';
const installed = '/Applications/Construct Browser.app/Contents/Resources/construct-agent';

describe('isTransientAppPath', () => {
  it.each([
    ['an App Translocation copy', translocated, true],
    ['a mounted disk image', mounted, true],
    ['the Applications folder', installed, false],
    [
      'an Applications folder nested deeper on a disk image',
      '/Volumes/Construct Browser/Sub/Applications/Construct Browser.app/Contents/Resources/construct-agent',
      true,
    ],
    [
      'an Applications folder on a second disk',
      '/Volumes/Data/Applications/Construct Browser.app/Contents/Resources/construct-agent',
      false,
    ],
    [
      'a Downloads folder',
      '/Users/a/Downloads/Construct Browser.app/Contents/Resources/construct-agent',
      false,
    ],
  ])('on macOS, %s: %s', (_name, binary, expected) => {
    expect(isTransientAppPath(binary, 'darwin')).toBe(expected);
  });

  it('is never true on another system', () => {
    expect(isTransientAppPath(mounted, 'linux')).toBe(false);
    expect(isTransientAppPath(translocated, 'win32')).toBe(false);
  });
});

describe('registrationDecision', () => {
  it('does not register the agent from a translocated or /Volumes path', () => {
    expect(registrationDecision(true, {}, translocated, 'darwin')).toBe('skip-transient-path');
    expect(registrationDecision(true, {}, mounted, 'darwin')).toBe('skip-transient-path');
  });

  it('registers the agent from Applications', () => {
    expect(registrationDecision(true, {}, installed, 'darwin')).toBe('register');
  });

  it('skips a development run that did not ask', () => {
    expect(registrationDecision(false, {}, installed, 'darwin')).toBe('skip-development');
  });
});
