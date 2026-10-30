import {
  useCallback,
  useEffect,
  useLayoutEffect,
  useRef,
  type ReactNode,
} from 'react';
import {
  Group,
  Panel,
  Separator,
  type PanelImperativeHandle,
} from 'react-resizable-panels';
import {
  SIDEBAR_DEFAULT_WIDTH,
  SIDEBAR_MIN_WIDTH,
  SIDEBAR_MAX_WIDTH,
  type SidebarId,
} from './workspaceSidebarWidths';

export const SIDEBAR_SEPARATOR_WIDTH = 4;

export interface WorkspaceSidebarGroupProps {
  side: SidebarId;
  width: number;
  onWidthChange: (width: number) => void;
  contentMinWidth: number;
  sidebar: ReactNode;
  children: ReactNode;
}

export function WorkspaceSidebarGroup({
  side,
  width,
  onWidthChange,
  contentMinWidth,
  sidebar,
  children,
}: WorkspaceSidebarGroupProps) {
  const panelRef = useRef<PanelImperativeHandle>(null);
  const separatorRef = useRef<HTMLDivElement>(null);
  const groupRef = useRef<HTMLDivElement>(null);
  const visible = sidebar !== null && sidebar !== false;
  const restoreWidth = useCallback(() => {
    if (visible) panelRef.current?.resize(width);
  }, [visible, width]);

  // Panel layouts are percentages internally. Restore the pixel preference when
  // parent geometry changes, without turning automatic geometry into a preference.
  useLayoutEffect(() => {
    let frame = requestAnimationFrame(restoreWidth);
    const element = groupRef.current;
    if (!element) return;
    const observer = new ResizeObserver(() => {
      cancelAnimationFrame(frame);
      frame = requestAnimationFrame(restoreWidth);
    });
    observer.observe(element);
    return () => {
      cancelAnimationFrame(frame);
      observer.disconnect();
    };
  }, [restoreWidth, contentMinWidth]);

  const commitWidth = useCallback(() => {
    const size = panelRef.current?.getSize();
    if (size) onWidthChange(size.inPixels);
  }, [onWidthChange]);

  useEffect(() => {
    // The library owns expanded pointer hit regions and window-level dragging.
    // Its active separator identifies the user action even outside the handle.
    const finish = () => {
      if (separatorRef.current?.getAttribute('data-separator') === 'active') {
        commitWidth();
      }
    };
    const lostButtons = (event: PointerEvent) => {
      if (event.buttons === 0) finish();
    };
    window.addEventListener('pointerup', finish, true);
    window.addEventListener('pointercancel', finish, true);
    window.addEventListener('pointermove', lostButtons, true);
    window.addEventListener('blur', finish);
    return () => {
      window.removeEventListener('pointerup', finish, true);
      window.removeEventListener('pointercancel', finish, true);
      window.removeEventListener('pointermove', lostButtons, true);
      window.removeEventListener('blur', finish);
    };
  }, [commitWidth]);

  const sidebarPanel = visible ? (
    <Panel
      id={`${side}-sidebar`}
      panelRef={panelRef}
      defaultSize={width}
      minSize={SIDEBAR_MIN_WIDTH}
      maxSize={SIDEBAR_MAX_WIDTH}
      className="h-full overflow-hidden"
    >
      {sidebar}
    </Panel>
  ) : null;
  const separator = visible ? (
    <Separator
      id={`${side}-sidebar-separator`}
      elementRef={separatorRef}
      aria-label={`Resize ${side} sidebar`}
      className="w-1 bg-transparent hover:bg-brand/50 focus-visible:bg-brand/50 focus-visible:outline-none transition-colors cursor-col-resize"
      style={{ width: SIDEBAR_SEPARATOR_WIDTH }}
      onKeyUp={(event) => {
        if (['ArrowLeft', 'ArrowRight', 'Home', 'End'].includes(event.key)) {
          commitWidth();
        }
      }}
      onDoubleClick={() => onWidthChange(SIDEBAR_DEFAULT_WIDTH)}
    />
  ) : null;

  return (
    <Group
      id={`${side}-sidebar-group`}
      elementRef={groupRef}
      orientation="horizontal"
      className="h-full w-full"
      style={{
        height: '100%',
        width: '100%',
        minWidth:
          contentMinWidth + (visible ? width + SIDEBAR_SEPARATOR_WIDTH : 0),
      }}
    >
      {side === 'left' && sidebarPanel}
      {side === 'left' && separator}
      <Panel
        id={`${side}-sidebar-content`}
        minSize={contentMinWidth}
        className="h-full min-w-0 overflow-hidden"
      >
        {children}
      </Panel>
      {side === 'right' && separator}
      {side === 'right' && sidebarPanel}
    </Group>
  );
}
