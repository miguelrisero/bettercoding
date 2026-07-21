import type { ReactNode } from 'react';
import { ChatEntryContainer } from './ChatEntryContainer';

export interface ChatAssistantMessageRenderProps {
  content: string;
  workspaceId?: string;
}

interface ChatAssistantMessageProps {
  content: string;
  workspaceId?: string;
  title?: ReactNode;
  attribution?: ReactNode;
  renderMarkdown: (props: ChatAssistantMessageRenderProps) => ReactNode;
}

export function ChatAssistantMessage({
  content,
  workspaceId,
  title,
  attribution,
  renderMarkdown,
}: ChatAssistantMessageProps) {
  if (attribution) {
    return (
      <ChatEntryContainer
        variant="assistant"
        title={title}
        headerRight={attribution}
        expanded
      >
        {renderMarkdown({ content, workspaceId })}
      </ChatEntryContainer>
    );
  }

  return renderMarkdown({ content, workspaceId });
}
