import {
  useCallback,
  useEffect,
  useLayoutEffect,
  useRef,
  type ReactNode,
} from 'react';
import { flushSync } from 'react-dom';
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
  const panelElementRef = useRef<HTMLDivElement>(null);
  const separatorRef = useRef<HTMLDivElement>(null);
  const groupRef = useRef<HTMLDivElement>(null);
  const initialWidth = useRef(width);
  const registered = useRef(false);
  const visible = sidebar !== null && sidebar !== false;
  const restoreWidth = useCallback(() => {
    const group = groupRef.current;
    if (visible && group) {
      const available = group.clientWidth - SIDEBAR_SEPARATOR_WIDTH;
      if (available > 0)
        panelRef.current?.resize(`${(width / available) * 100}%`);
    }
  }, [visible, width]);

  // Panel layouts are percentages internally. Restore the pixel preference when
  // parent geometry changes, without turning automatic geometry into a preference.
  useLayoutEffect(() => {
    const frame = requestAnimationFrame(restoreWidth);
    const element = groupRef.current;
    if (!element) return;
    let disposed = false;
    const observer = new ResizeObserver(() => {
      // Restore during observer delivery, before paint. Panel observation also
      // catches constraint normalization that follows the group callback.
      if (separatorRef.current?.getAttribute('data-separator') !== 'active') {
        queueMicrotask(() => {
          if (!disposed) flushSync(restoreWidth);
        });
      }
    });
    observer.observe(element);
    if (panelElementRef.current) observer.observe(panelElementRef.current);
    if (registered.current) {
      // Registration can restore cached percentages without changing geometry.
      queueMicrotask(() => {
        if (!disposed) flushSync(restoreWidth);
      });
    }
    registered.current = true;
    return () => {
      disposed = true;
      cancelAnimationFrame(frame);
      observer.disconnect();
    };
  }, [restoreWidth, contentMinWidth, visible, width]);

  const commitWidth = useCallback(() => {
    const size = panelRef.current?.getSize();
    const group = groupRef.current;
    if (size && group) {
      // The library store updates before React commits the panel's DOM width.
      const available = group.clientWidth - SIDEBAR_SEPARATOR_WIDTH;
      onWidthChange((size.asPercentage / 100) * available);
    }
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
      elementRef={panelElementRef}
      defaultSize={initialWidth.current}
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
      onKeyDown={(event) => {
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
