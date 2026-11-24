import { t } from '../translation';

/** A tool's name for people; one the app has no name for is shown as the agent sent it. */
export function connectorName(name: string): string {
  return name === 'claude-code'
    ? t('StatusPage.Connector.claude-code')
    : name === 'codex'
      ? t('StatusPage.Connector.codex')
      : name === 'chatgpt'
        ? t('StatusPage.Connector.chatgpt')
        : name;
}
