import { useState, type ReactNode, type SyntheticEvent } from 'react';
import { GitBranchIcon } from '@phosphor-icons/react';

import { Badge } from './Badge';

export interface ChatForkBranch {
  id: string;
  label: string;
  isDefault: boolean;
  content: ReactNode;
}

interface ChatForkBranchesProps {
  explanation: string;
  resumeHint: string;
  emptyBranchLabel: string;
  branches: ChatForkBranch[];
}

function ForkBranch({
  branch,
  resumeHint,
  emptyBranchLabel,
}: {
  branch: ChatForkBranch;
  resumeHint: string;
  emptyBranchLabel: string;
}) {
  const [open, setOpen] = useState(branch.isDefault);
  const handleToggle = (event: SyntheticEvent<HTMLDetailsElement>) => {
    setOpen(event.currentTarget.open);
  };

  return (
    <details open={open} onToggle={handleToggle}>
      <summary className="flex min-h-11 cursor-pointer list-none items-center gap-base px-double py-base text-sm text-normal transition-colors hover:bg-secondary/60 focus-visible:outline-none focus-visible:ring-1 focus-visible:ring-inset focus-visible:ring-ring/40 [&::-webkit-details-marker]:hidden">
        <span className="min-w-0 flex-1 truncate font-medium">
          {branch.label}
        </span>
        {branch.isDefault && (
          <Badge
            variant="outline"
            className="shrink-0 border-border bg-primary px-base py-half font-normal text-low"
          >
            {resumeHint}
          </Badge>
        )}
      </summary>
      <div className="border-t border-border bg-primary/40 py-base">
        {branch.content ?? (
          <p className="px-double py-base text-sm text-low">
            {emptyBranchLabel}
          </p>
        )}
      </div>
    </details>
  );
}

export function ChatForkBranches({
  explanation,
  resumeHint,
  emptyBranchLabel,
  branches,
}: ChatForkBranchesProps) {
  return (
    <section className="overflow-hidden rounded-sm border border-border bg-secondary/20">
      <div className="flex items-start gap-base border-b border-border px-double py-base">
        <GitBranchIcon className="mt-0.5 size-icon-base shrink-0 text-low" />
        <p className="text-sm leading-relaxed text-normal">{explanation}</p>
      </div>
      <div className="divide-y divide-border">
        {branches.map((branch) => (
          <ForkBranch
            key={branch.id}
            branch={branch}
            resumeHint={resumeHint}
            emptyBranchLabel={emptyBranchLabel}
          />
        ))}
      </div>
    </section>
  );
}
