import type { ButtonHTMLAttributes, ReactNode } from 'react';
import type { Icon } from '@phosphor-icons/react';
import {
  Layout as LayoutIcon,
  ChatsTeardrop as ChatsTeardropIcon,
  GitDiff as GitDiffIcon,
  Terminal as TerminalIcon,
  Desktop as DesktopIcon,
  GitFork as GitForkIcon,
  List as ListIcon,
  Gear as GearIcon,
  CaretLeft as CaretLeftIcon,
  ArrowClockwise as ArrowClockwiseIcon,
} from '@phosphor-icons/react';
import { cn } from '../lib/cn';
import { Tooltip } from './Tooltip';
import {
  SyncErrorIndicator,
  type SyncErrorIndicatorError,
} from './SyncErrorIndicator';

/**
 * Action item rendered in the navbar.
 */
export interface NavbarActionItem {
  type?: 'action';
  id: string;
  icon: Icon;
  isActive?: boolean;
  tooltip?: string;
  shortcut?: string;
  disabled?: boolean;
  onClick?: () => void;
}

/**
 * Divider item rendered in the navbar.
 */
export interface NavbarDividerItem {
  type: 'divider';
}

export type NavbarSectionItem = NavbarActionItem | NavbarDividerItem;

function isDivider(item: NavbarSectionItem): item is NavbarDividerItem {
  return item.type === 'divider';
}

// NavbarIconButton - inlined from primitives
interface NavbarIconButtonProps
  extends ButtonHTMLAttributes<HTMLButtonElement> {
  icon: Icon;
  isActive?: boolean;
  tooltip?: string;
  shortcut?: string;
}

function NavbarIconButton({
  icon: IconComponent,
  isActive = false,
  tooltip,
  shortcut,
  className,
  ...props
}: NavbarIconButtonProps) {
  const button = (
    <button
      type="button"
      className={cn(
        'flex items-center justify-center rounded-sm',
        'text-low hover:text-normal',
        isActive && 'text-normal',
        className
      )}
      {...props}
    >
      <IconComponent
        className="size-icon-base"
        weight={isActive ? 'fill' : 'regular'}
      />
    </button>
  );

  return tooltip ? (
    <Tooltip content={tooltip} shortcut={shortcut}>
      {button}
    </Tooltip>
  ) : (
    button
  );
}

export type MobileTabId =
  | 'workspaces'
  | 'chat'
  | 'changes'
  | 'logs'
  | 'preview'
  | 'git';

export const MOBILE_TABS: { id: MobileTabId; icon: Icon; label: string }[] = [
  { id: 'workspaces', icon: LayoutIcon, label: 'Wksps' },
  { id: 'chat', icon: ChatsTeardropIcon, label: 'Chat' },
  { id: 'changes', icon: GitDiffIcon, label: 'Diff' },
  { id: 'logs', icon: TerminalIcon, label: 'Logs' },
  { id: 'preview', icon: DesktopIcon, label: 'Preview' },
  { id: 'git', icon: GitForkIcon, label: 'Git' },
];

export interface NavbarBreadcrumbItem {
  label: string;
  onClick?: () => void;
}

interface NavbarBreadcrumbsProps {
  breadcrumbs: NavbarBreadcrumbItem[];
  textClassName: string;
}

function NavbarBreadcrumbs({
  breadcrumbs,
  textClassName,
}: NavbarBreadcrumbsProps) {
  return (
    <div className={cn('flex items-center gap-1 min-w-0', textClassName)}>
      {breadcrumbs.map((crumb, index) => {
        const isLast = index === breadcrumbs.length - 1;
        return (
          <span key={index} className="flex items-center gap-1 min-w-0">
            {index > 0 && <span className="text-low shrink-0">/</span>}
            {crumb.onClick && !isLast ? (
              <button
                type="button"
                className="text-low hover:text-normal truncate cursor-pointer"
                onClick={crumb.onClick}
              >
                {crumb.label}
              </button>
            ) : (
              <span
                className={cn('truncate', isLast ? 'text-normal' : 'text-low')}
              >
                {crumb.label}
              </span>
            )}
          </span>
        );
      })}
    </div>
  );
}

export interface NavbarProps {
  workspaceTitle?: string;
  breadcrumbs?: NavbarBreadcrumbItem[];
  // Brand wordmark rendered as the left-most element (desktop)
  brand?: ReactNode;
  // Items for left side of navbar
  leftItems?: NavbarSectionItem[];
  // Items for right side of navbar (with dividers inline)
  rightItems?: NavbarSectionItem[];
  // Optional additional content for left side (after leftItems)
  leftSlot?: ReactNode;
  // Left-most content of the right section (e.g. app version)
  rightStart?: ReactNode;
  // Right-most content of the right section (e.g. notifications, user menu)
  rightEnd?: ReactNode;
  // Sync errors shown in the right section
  syncErrors?: readonly SyncErrorIndicatorError[] | null;
  className?: string;
  // Mobile props
  mobileMode?: boolean;
  mobileUserSlot?: ReactNode;
  /** Extra buttons for the mobile top-right cluster (e.g. CLI mode toggle). */
  mobileActionsSlot?: ReactNode;
  onOpenCommandBar?: () => void;
  onOpenSettings?: () => void;
  onNavigateBack?: () => void;
  onReload?: () => void;
  mobileActiveTab?: MobileTabId;
  onMobileTabChange?: (tab: MobileTabId) => void;
  mobileTabs?: { id: MobileTabId; icon: Icon; label: string }[];
  showMobileTabs?: boolean;
  mobileShowBack?: boolean;
}

export function Navbar({
  workspaceTitle,
  breadcrumbs,
  brand,
  leftItems = [],
  rightItems = [],
  leftSlot,
  rightStart,
  rightEnd,
  syncErrors,
  className,
  mobileMode = false,
  mobileUserSlot,
  mobileActionsSlot,
  onOpenCommandBar,
  onOpenSettings,
  onNavigateBack,
  onReload,
  mobileActiveTab = 'chat',
  onMobileTabChange,
  mobileTabs,
  showMobileTabs,
  mobileShowBack,
}: NavbarProps) {
  const renderItem = (item: NavbarSectionItem, key: string) => {
    // Render divider
    if (isDivider(item)) {
      return <div key={key} className="h-4 w-px bg-border" />;
    }

    const isDisabled = !!item.disabled;

    return (
      <NavbarIconButton
        key={key}
        icon={item.icon}
        isActive={item.isActive}
        onClick={item.onClick}
        aria-label={item.tooltip}
        tooltip={item.tooltip}
        shortcut={item.shortcut}
        disabled={isDisabled}
        className={isDisabled ? 'opacity-40 cursor-not-allowed' : ''}
      />
    );
  };

  // ---- Mobile layout ----
  if (mobileMode) {
    return (
      <nav
        className={cn(
          'flex flex-col bg-secondary border-b shrink-0',
          className
        )}
      >
        {/* Row 1: Tab bar */}
        <div className="flex items-center justify-between px-base py-half">
          <div className="flex items-center gap-0.5 overflow-x-auto">
            {mobileShowBack && onNavigateBack && (
              <>
                <button
                  type="button"
                  className="flex items-center justify-center px-1.5 py-1 text-low hover:text-normal"
                  onClick={onNavigateBack}
                  aria-label="Back"
                >
                  <CaretLeftIcon className="size-icon-sm" />
                </button>
                <div className="h-4 w-px bg-border mx-0.5 shrink-0" />
              </>
            )}
            {showMobileTabs !== false &&
              (mobileTabs ?? MOBILE_TABS).map((tab) => {
                const TabIcon = tab.icon;
                const isActive = mobileActiveTab === tab.id;
                return (
                  <button
                    key={tab.id}
                    type="button"
                    className={cn(
                      'flex items-center gap-1 px-1.5 py-1 text-xs whitespace-nowrap transition-colors',
                      isActive
                        ? 'text-normal border-b-2 border-brand'
                        : 'text-low hover:text-normal'
                    )}
                    onClick={() => onMobileTabChange?.(tab.id)}
                  >
                    <TabIcon
                      className="size-icon-sm"
                      weight={isActive ? 'fill' : 'regular'}
                    />
                    <span className="hidden min-[480px]:inline">
                      {tab.label}
                    </span>
                  </button>
                );
              })}
          </div>

          {/* Right side: sync indicator + action buttons + user slot */}
          <div className="flex items-center gap-1 shrink-0">
            <SyncErrorIndicator errors={syncErrors} />
            {mobileActionsSlot}
            {onReload && (
              <button
                type="button"
                className="flex items-center justify-center text-low hover:text-normal"
                onClick={onReload}
                aria-label="Reload"
              >
                <ArrowClockwiseIcon className="size-icon-sm" />
              </button>
            )}
            {onOpenSettings && (
              <button
                type="button"
                className="flex items-center justify-center text-low hover:text-normal"
                onClick={onOpenSettings}
                aria-label="Settings"
              >
                <GearIcon className="size-icon-sm" />
              </button>
            )}
            {onOpenCommandBar && (
              <button
                type="button"
                className="flex items-center justify-center text-low hover:text-normal"
                onClick={onOpenCommandBar}
                aria-label="Command bar"
              >
                <ListIcon className="size-icon-sm" />
              </button>
            )}
            {mobileUserSlot && (
              <div className="h-4 w-px bg-border mx-0.5 shrink-0" />
            )}
            {mobileUserSlot}
          </div>
        </div>

        {/* Row 2: Info bar with leftSlot + breadcrumbs/title */}
        {(workspaceTitle || breadcrumbs) && (
          <div className="flex items-center justify-between px-base py-half border-t border-border">
            <div className="flex items-center gap-base flex-1 min-w-0">
              {leftSlot}
              {breadcrumbs && breadcrumbs.length > 0 ? (
                <NavbarBreadcrumbs
                  breadcrumbs={breadcrumbs}
                  textClassName="text-sm"
                />
              ) : (
                <p className="text-sm text-low truncate cursor-default select-none">
                  {workspaceTitle}
                </p>
              )}
            </div>
          </div>
        )}
      </nav>
    );
  }

  // ---- Desktop layout ----
  // data-tauri-drag-region must be on every non-interactive element for Tauri 2
  // window dragging to work (the attribute does not propagate to children).
  return (
    <nav
      data-tauri-drag-region
      className={cn(
        'flex items-center justify-between px-base py-half bg-secondary border-b shrink-0',
        className
      )}
    >
      {/* Left - Brand + actions + optional slot */}
      <div data-tauri-drag-region className="flex-1 flex items-center gap-base">
        {brand}
        {leftItems.map((item, index) =>
          renderItem(
            item,
            `left-${isDivider(item) ? 'divider' : item.id}-${index}`
          )
        )}
        {leftSlot}
      </div>

      {/* Center - Breadcrumbs or Workspace Title */}
      <div
        data-tauri-drag-region
        className="flex-1 flex items-center justify-center min-w-0"
      >
        {breadcrumbs && breadcrumbs.length > 0 ? (
          <NavbarBreadcrumbs
            breadcrumbs={breadcrumbs}
            textClassName="text-base"
          />
        ) : (
          <p
            data-tauri-drag-region
            className="text-base text-low truncate cursor-default select-none"
          >
            {workspaceTitle ?? ''}
          </p>
        )}
      </div>

      {/* Right - version + sync indicator + panel toggles + user area */}
      <div
        data-tauri-drag-region
        className="flex-1 flex items-center justify-end gap-base"
      >
        {rightStart}
        <SyncErrorIndicator errors={syncErrors} />
        {rightItems.map((item, index) =>
          renderItem(
            item,
            `right-${isDivider(item) ? 'divider' : item.id}-${index}`
          )
        )}
        {rightEnd}
      </div>
    </nav>
  );
}
