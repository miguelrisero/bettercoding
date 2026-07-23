import type { UnassignedCliSession } from 'shared/types';

export interface PartitionedCliSessions {
  main: UnassignedCliSession[];
  agents: UnassignedCliSession[];
}

/**
 * Split unassigned CLI sessions into visible "main" conversations and hidden
 * background "agents". FAIL OPEN TO MAIN: a session is treated as a background
 * agent ONLY when it is positively kind === 'subagent'. Anything else — an
 * unrecognized/absent kind from an older payload — stays a visible main
 * conversation. This mirrors the backend invariant; never hide a real chat.
 */
export function partitionCliSessionsByKind(
  sessions: UnassignedCliSession[]
): PartitionedCliSessions {
  const main: UnassignedCliSession[] = [];
  const agents: UnassignedCliSession[] = [];
  for (const session of sessions) {
    if (session.kind === 'subagent') {
      agents.push(session);
    } else {
      main.push(session);
    }
  }
  return { main, agents };
}
