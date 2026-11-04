/**
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

import type {JSX, MouseEvent, ReactNode} from 'react';
import type {ExclusiveOr} from './Types';

import React, {useEffect, useLayoutEffect, useRef, useState} from 'react';
import {ViewportOverlay} from './ViewportOverlay';
import {findParentWithClassName} from './utils';
import {getZoomLevel} from './zoom';

import './Tooltip.css';

export type Placement = 'top' | 'bottom' | 'left' | 'right';

/**
 * Default delay used for hover tooltips to convey documentation information.
 */
export const DOCUMENTATION_DELAY = 750;

const tooltipGroups: Map<string, EventTarget> = new Map();
function getTooltipGroup(group: string): EventTarget {
  let found = tooltipGroups.get(group);
  if (found == null) {
    found = new EventTarget();
    tooltipGroups.set(group, found);
  }
  return found;
}

/**
 * Useful for passing custom props to a usage of Tooltip that you don't directly control.
 */
export type TooltipProps = {
  inline?: boolean;
  placement?: Placement;
  /**
   * Applies delay to visual appearance of tooltip.
   * Note element is always constructed immediately.
   * This delay applies to all trigger methods except 'click'.
   * The delay is only on the leading-edge; disappearing is always instant.
   */
  delayMs?: number;
  /**
   * When true, the tooltip stays visible when the mouse moves from the trigger
   * element onto the tooltip content itself. This allows users to interact with
   * the tooltip content, such as selecting and copying text.
   * Only applies to hover-triggered tooltips with a `component` prop.
   */
  interactive?: boolean;
  /**
   * Callback to run when the tooltip becomes visible.
   * For 'click' tooltips that also have a 'title', this only fires when the 'click' tooltip is shown.
   */
  onVisible?: () => unknown;
  /**
   * Callback to run when the tooltip is dismissed for any reason.
   * For 'click' tooltips that also have a 'title', this only fires when the 'click' tooltip is dismissed.
   * Note: `onDismiss` will not run if the entire <Tooltip> is unmounted while the tooltip is visible.
   */
  onDismiss?: () => unknown;
} & ExclusiveOr<
  ExclusiveOr<{trigger: 'manual'; shouldShow: boolean}, {trigger?: 'hover' | 'disabled'}> &
    ExclusiveOr<
      {component: (dismiss: () => void) => JSX.Element},
      {title: string | React.ReactNode}
    >,
  {
    trigger: 'click';
    component: (dismiss: () => void) => JSX.Element;
    title?: string | React.ReactNode;
    /** If provided, opening this tooltip will close all other tooltips using the same group */
    group?: string;
    /** Event Emitter that allows external callers to toggle this tooltip.
     * See also `shared/TypedEventEmitter`'s .asEventTarget() to get a typed API. */
    additionalToggles?: EventTarget;
  }
>;

type TooltipPropsWithChildren = {children: ReactNode} & TooltipProps;

type VisibleState =
  | true /* primary content (prefers component) is visible */
  | false
  | 'title' /* 'title', not 'component' is visible */;

class TooltipChangeEvent<T> extends Event {
  constructor(
    type: string,
    public data: T | Error,
  ) {
    super(type);
  }
}

/**
 * Enables child elements to render a tooltip when hovered/clicked.
 * Children are always rendered, but the tooltip is not rendered until triggered.
 * Tooltip is centered on bounding box of children.
 * You can adjust the trigger method:
 *  - 'hover' (default) to appear when mouse hovers container element
 *  - 'click' to render `component` on click, render `title` on hover.
 *  - 'manual' to control programmatically by providing `shouldShow` prop.
 *  - 'disabled' to turn off hover/click support programmatically
 *
 * You can adjust which side the tooltip appears on.
 *   Default placement is 'top', above the element.
 *
 * Tooltip content may either be a (i18n-ized) string `title`, or a `Component` to render.
 *   `title`s will automatically wrap text,
 *   but `Component`s are expected to handle their own sizing.
 * `Component`-rendered content allows pointer events inside the tooltip,
 *   but string `title`s do not allow pointer events, and dismiss if the mouse exits
 *   the original tooltip creator.
 *
 * Tooltips will hide themselves when you scroll or resize.
 * This applies even to manual tooltips with shouldShow=true.
 */
export function Tooltip({
  children,
  title,
  component,
  placement: placementProp,
  trigger: triggerProp,
  delayMs,
  shouldShow,
  onVisible,
  onDismiss,
  additionalToggles,
  group,
  inline: inlineProp,
  interactive,
}: TooltipPropsWithChildren) {
  const inline = inlineProp ?? false;
  const trigger = triggerProp ?? 'hover';
  const placement = placementProp ?? 'top';
  const [visible, setVisible] = useState<VisibleState>(false);
  const isHoveringTooltipContent = useRef(false);

  // trigger onDismiss when visibility newly becomes false
  const lastVisible = useRef(false);
  useEffect(() => {
    if (!visible && lastVisible.current === true) {
      onDismiss?.();
    }
    lastVisible.current = visible === true;
  }, [visible, onDismiss, lastVisible]);

  // trigger onVisible when visibility newly becomes true
  useEffect(() => {
    if (visible === true) {
      onVisible?.();
    }
  }, [visible, onVisible]);

  const ref = useRef<HTMLDivElement>(null);
  // The portaled tooltip content div, so handlers can tell whether events or
  // focus belong to this tooltip instance rather than any tooltip on the page.
  const contentRef = useRef<HTMLDivElement | null>(null);
  const getContent = () => {
    if (visible === 'title') {
      return title;
    }
    return component == null ? title : component(() => setVisible(false));
  };

  useEffect(() => {
    if (typeof shouldShow === 'boolean') {
      setVisible(shouldShow);
    }
  }, [shouldShow]);

  useEffect(() => {
    if (trigger === 'click') {
      if (visible) {
        // When using click trigger, we need to listen for clicks outside the tooltip
        // to close it again.
        const globalClickHandler = (e: Event) => {
          if (!eventIsFromInsideTooltip(e as unknown as MouseEvent)) {
            setVisible(false);
          }
        };
        window.addEventListener('click', globalClickHandler);
        return () => window.removeEventListener('click', globalClickHandler);
      }
    }
  }, [visible, setVisible, trigger]);

  useEffect(() => {
    const cb = () => setVisible(last => !last);
    additionalToggles?.addEventListener('change', cb);
    return () => {
      additionalToggles?.removeEventListener('change', cb);
    };
  }, [additionalToggles]);

  useEffect(() => {
    if (group) {
      const hide = (event: Event) => {
        if ((event as TooltipChangeEvent<HTMLElement>).data === ref.current) {
          // don't hide the tooltip we're trying to show right now
          return;
        }
        setVisible(false);
      };
      const found = getTooltipGroup(group);
      found.addEventListener('change', hide);
      return () => {
        found?.removeEventListener('change', hide);
      };
    }
  }, [group]);

  // scrolling or resizing the window should hide all tooltips to prevent lingering.
  useEffect(() => {
    if (visible) {
      const hideTooltip = (e: Event) => {
        if (e.type === 'keyup') {
          if ((e as KeyboardEvent).key === 'Escape') {
            setVisible(false);
          }
        } else if (e.type === 'resize') {
          setVisible(false);
        } else if (
          e.target !== document &&
          e.target !== document.documentElement &&
          !eventIsFromInsideTooltip(e as unknown as MouseEvent) &&
          // Typing in a tooltip's input can shrink content elsewhere on the page,
          // firing a scrollTop-clamp scroll event. Don't dismiss out from under
          // the user while focus is inside this tooltip.
          !(contentRef.current?.contains(document.activeElement) ?? false)
        ) {
          setVisible(false);
        }
      };
      window.addEventListener('scroll', hideTooltip, true);
      window.addEventListener('resize', hideTooltip, true);
      window.addEventListener('keyup', hideTooltip, true);
      return () => {
        window.removeEventListener('scroll', hideTooltip, true);
        window.removeEventListener('resize', hideTooltip, true);
        window.removeEventListener('keyup', hideTooltip, true);
      };
    }
  }, [visible, setVisible]);

  // Using onMouseLeave directly on the div is unreliable if the component rerenders: https://github.com/facebook/react/issues/4492
  // Use a manually managed subscription instead.
  useLayoutEffect(() => {
    const needHover = trigger === 'hover' || (trigger === 'click' && title != null);
    if (!needHover) {
      return;
    }
    // Do not change visible if 'click' shows the content.
    const onMouseEnter = () =>
      setVisible(vis => (trigger === 'click' ? (vis === true ? vis : 'title') : true));
    const onMouseLeave = () => {
      // For interactive tooltips, delay hiding to allow mouse to move to tooltip content
      if (interactive && component != null) {
        setTimeout(() => {
          if (!isHoveringTooltipContent.current) {
            setVisible(vis => (trigger === 'click' && vis === true ? vis : false));
          }
        }, 100);
      } else {
        setVisible(vis => (trigger === 'click' && vis === true ? vis : false));
      }
    };
    const div = ref.current;
    div?.addEventListener('mouseenter', onMouseEnter);
    div?.addEventListener('mouseleave', onMouseLeave);
    return () => {
      div?.removeEventListener('mouseenter', onMouseEnter);
      div?.removeEventListener('mouseleave', onMouseLeave);
    };
  }, [trigger, title, interactive, component]);

  // Force delayMs to be 0 when `component` is shown by click.
  const realDelayMs = trigger === 'click' && visible === true ? 0 : delayMs;

  return (
    <div
      className={inline ? 'tooltip-creator-inline' : 'tooltip-creator'}
      ref={ref}
      onClick={
        trigger === 'click'
          ? (event: MouseEvent) => {
              if (visible !== true || !eventIsFromInsideTooltip(event)) {
                if (group != null) {
                  // close other tooltips in the same group before opening
                  const found = getTooltipGroup(group);
                  found.dispatchEvent(new TooltipChangeEvent('change', ref.current));
                }
                setVisible(vis => vis !== true);
                // don't trigger global click listener in the same tick
                event.stopPropagation();
              }
            }
          : undefined
      }>
      {visible && ref.current && (
        <RenderTooltipOnto
          delayMs={realDelayMs}
          tooltipRef={contentRef}
          element={ref.current}
          placement={placement}
          interactive={interactive}
          onTooltipMouseEnter={() => {
            isHoveringTooltipContent.current = true;
          }}
          onTooltipMouseLeave={() => {
            isHoveringTooltipContent.current = false;
            if (trigger === 'hover') {
              setVisible(false);
            }
          }}>
          {getContent()}
        </RenderTooltipOnto>
      )}
      {children}
    </div>
  );
}

/**
 * If you click inside a tooltip triggered by click, we don't want to dismiss the tooltip.
 * We consider any click in a descendant of ANY tooltip as a click.
 * Same applies for scroll events inside tooltips.
 */
function eventIsFromInsideTooltip(event: MouseEvent): boolean {
  const parentTooltip = findParentWithClassName(event.target as HTMLElement, 'tooltip');
  return parentTooltip != null;
}

function RenderTooltipOnto({
  element,
  placement,
  children,
  delayMs,
  interactive,
  tooltipRef,
  onTooltipMouseEnter,
  onTooltipMouseLeave,
}: {
  element: HTMLElement;
  placement: Placement;
  children: ReactNode;
  delayMs?: number;
  interactive?: boolean;
  tooltipRef: React.MutableRefObject<HTMLDivElement | null>;
  onTooltipMouseEnter?: () => void;
  onTooltipMouseLeave?: () => void;
}) {
  const sourceBoundingRect = element.getBoundingClientRect();

  const zoom = getZoomLevel();
  let effectivePlacement = placement;
  const viewportDimensions = document.body.getBoundingClientRect();
  viewportDimensions.width /= zoom;
  viewportDimensions.height /= zoom;

  // to center the tooltip over the tooltip-creator, we need to measure its final rendered size
  const renderedDimensions = useRenderedDimensions(tooltipRef, children);
  const position = offsetsForPlacement(
    placement,
    sourceBoundingRect,
    renderedDimensions,
    viewportDimensions,
  );
  effectivePlacement = position.autoPlacement ?? placement;
  // The tooltip may end up overflowing off the screen, since it's rendered absolutely.
  // We can push it back as needed with an additional offset.
  const viewportAdjust = getViewportAdjustedDelta(effectivePlacement, position, {
    width: renderedDimensions.width,
    height: Math.min(renderedDimensions.height, position.maxHeight ?? Infinity),
  });

  const style: React.CSSProperties = {
    animationDelay: delayMs ? `${delayMs}ms` : undefined,
  };

  const pad = 10;

  // Always use style.left/top for positioning. width: max-content prevents the
  // browser from squishing the tooltip when positioned near the right edge,
  // which previously required switching between style.left and style.right
  // and caused dimension oscillation with ResizeObserver.
  style.left = position.left + viewportAdjust.left;
  style.top = position.top + viewportAdjust.top;
  style.width = 'max-content';
  style.maxWidth = viewportDimensions.width - 2 * pad;
  style.boxSizing = 'border-box';
  const contentStyle: React.CSSProperties = {overflowY: 'auto'};
  if (position.maxHeight != null) {
    contentStyle.maxHeight = Math.max(position.maxHeight - renderedDimensions.chromeHeight, 0);
  }

  // Use a portal so the tooltip element is rendered into the global list of tooltips,
  // rather than as a descendant of the tooltip creator.
  // This allows us to use absolute coordinates for positioning, and for
  // tooltips to "escape" their containing elements, scroll, inherited styles, etc.
  return (
    <ViewportOverlay>
      <div
        ref={tooltipRef}
        role="tooltip"
        className={
          `tooltip tooltip-with-content tooltip-${effectivePlacement}` +
          (typeof children === 'string' ? ' simple-text-tooltip' : '')
        }
        style={style}
        onMouseEnter={interactive ? onTooltipMouseEnter : undefined}
        onMouseLeave={interactive ? onTooltipMouseLeave : undefined}>
        <div
          className={`tooltip-arrow tooltip-arrow-${effectivePlacement}`}
          // If we had to push the tooltip back to prevent overflow,
          // we also need to move the arrow the opposite direction so it still lines up.
          style={{transform: `translate(${-viewportAdjust.left}px, ${-viewportAdjust.top}px)`}}
        />
        {/* Keep the arrow outside the scroll container so its overflow does not create a scrollbar. */}
        <div className="tooltip-content" style={contentStyle}>
          {children}
        </div>
      </div>
    </ViewportOverlay>
  );
}

type OffsetPlacement = {
  top: number;
  left: number;
  autoPlacement?: Placement;
  /** Max border-box height of the tooltip, set when it would not otherwise fit in the viewport. */
  maxHeight?: number;
};

/**
 * Offset tooltip from tooltipCreator's absolute position using `placement`,
 * such that it is centered and on the correct side.
 * This requires the rendered tooltip's width and height (for centering).
 * Coordinates are left&top absolute offsets.
 *
 * When appropriate, we also detect if this placement would go offscreen
 * and instead provide a better placement.
 *
 * In this diagram, `0` is `tooltipCreatorRect`,
 * `1` is what we want to compute using 0 and the size of the rendered tooltip.
 *
 *     0---*
 *     |   |     <- tooltip creator (The thing you hover to see the tooltip)
 *     *---*
 *       ^       <- tooltip arrow
 *  1---------+
 *  |         |  <- tooltip
 *  +---------+
 *
 */
function offsetsForPlacement(
  placement: Placement,
  tooltipCreatorRect: DOMRect,
  tooltipDimensions: {width: number; height: number},
  viewportDimensions: DOMRect,
): OffsetPlacement {
  const padding = 5;
  let result: OffsetPlacement = {top: 0, left: 0};
  let currentPlacement = placement;
  for (let i = 0; i <= 2; i++) {
    switch (currentPlacement) {
      case 'top': {
        result = {
          top: tooltipCreatorRect.top - padding - tooltipDimensions.height,
          left:
            tooltipCreatorRect.left + tooltipCreatorRect.width / 2 - tooltipDimensions.width / 2,
        };
        if (result.top < 0) {
          currentPlacement = 'bottom';
          continue;
        }
        break;
      }
      case 'bottom': {
        result = {
          top: tooltipCreatorRect.top + tooltipCreatorRect.height + padding,
          left:
            tooltipCreatorRect.left + tooltipCreatorRect.width / 2 - tooltipDimensions.width / 2,
        };
        if (result.top + tooltipDimensions.height > viewportDimensions.height) {
          currentPlacement = 'top';
          continue;
        }
        break;
      }
      case 'left': {
        result = {
          top:
            tooltipCreatorRect.top + tooltipCreatorRect.height / 2 - tooltipDimensions.height / 2,
          left: tooltipCreatorRect.left - tooltipDimensions.width - padding,
        };
        if (result.left < 0) {
          currentPlacement = 'right';
          continue;
        }
        break;
      }
      case 'right': {
        result = {
          top:
            tooltipCreatorRect.top + tooltipCreatorRect.height / 2 - tooltipDimensions.height / 2,
          left: tooltipCreatorRect.right + padding,
        };
        if (result.left + tooltipDimensions.width > viewportDimensions.width) {
          currentPlacement = 'left';
          continue;
        }
        break;
      }
    }
    break;
  }

  const viewportPadding = 5;
  if (currentPlacement === 'top' || currentPlacement === 'bottom') {
    const roomAbove = tooltipCreatorRect.top - padding - viewportPadding;
    const roomBelow =
      viewportDimensions.height -
      (tooltipCreatorRect.top + tooltipCreatorRect.height + padding) -
      viewportPadding;
    if (tooltipDimensions.height > roomAbove && tooltipDimensions.height > roomBelow) {
      // Fits on neither side: use the side with more room, and make the content scroll.
      currentPlacement = roomAbove > roomBelow ? 'top' : 'bottom';
      const maxHeight = Math.max(currentPlacement === 'top' ? roomAbove : roomBelow, 0);
      result = {
        top:
          currentPlacement === 'top'
            ? tooltipCreatorRect.top - padding - maxHeight
            : tooltipCreatorRect.top + tooltipCreatorRect.height + padding,
        left: result.left,
        maxHeight,
      };
    }
  } else {
    const maxHeight = Math.max(viewportDimensions.height - 2 * viewportPadding, 0);
    if (tooltipDimensions.height > maxHeight) {
      result.maxHeight = maxHeight;
    }
  }

  // Set autoPlacement if we chose a different placement.
  if (currentPlacement !== placement) {
    result.autoPlacement = currentPlacement;
  }
  return result;
}

/**
 * If the rendered tooltip would overflow outside the screen bounds,
 * we need to translate the tooltip back into bounds.
 */
function getViewportAdjustedDelta(
  placement: Placement,
  pos: {top: number; left: number},
  renderedDimensions: {width: number; height: number},
): {left: number; top: number} {
  const delta = {top: 0, left: 0};

  const viewportPadding = 5;
  const viewportDimensions = document.body.getBoundingClientRect();

  const zoom = getZoomLevel();
  viewportDimensions.width /= zoom;
  viewportDimensions.height /= zoom;

  if (placement === 'right' || placement === 'left') {
    const topEdgeOffset = pos.top - viewportPadding;
    const bottomEdgeOffset = pos.top + viewportPadding + renderedDimensions.height;
    if (topEdgeOffset < viewportDimensions.top) {
      // top overflow
      delta.top = viewportDimensions.top - topEdgeOffset;
    } else if (bottomEdgeOffset > viewportDimensions.top + viewportDimensions.height) {
      // bottom overflow
      delta.top = viewportDimensions.top + viewportDimensions.height - bottomEdgeOffset;
    }
  } else {
    const leftEdgeOffset = pos.left - viewportPadding;
    const rightEdgeOffset = pos.left + viewportPadding + renderedDimensions.width;
    if (leftEdgeOffset < viewportDimensions.left) {
      // left overflow
      delta.left = viewportDimensions.left - leftEdgeOffset;
    } else if (rightEdgeOffset > viewportDimensions.right) {
      // right overflow
      delta.left = viewportDimensions.left + viewportDimensions.width - rightEdgeOffset;
    }
  }

  return delta;
}

function useRenderedDimensions(ref: React.MutableRefObject<HTMLDivElement | null>, deps: unknown) {
  const [dimensions, setDimensions] = useState({width: 0, height: 0, chromeHeight: 0});

  useLayoutEffect(() => {
    const target = ref.current;
    if (target == null) {
      return;
    }

    const observer = new ResizeObserver(() => updateDimensions());
    const observedChildren = new Set<Element>();

    const updateDimensions = () => {
      const content = target.querySelector<HTMLElement>(':scope > .tooltip-content');
      // Report the natural height, including any part of the content scrolled out of view,
      // so that capping the content's height does not feed back into placement.
      const hiddenContentHeight =
        content == null ? 0 : Math.max(content.scrollHeight - content.clientHeight, 0);
      const next = {
        width: target.offsetWidth,
        height: target.offsetHeight + hiddenContentHeight,
        chromeHeight: content == null ? 0 : target.offsetHeight - content.offsetHeight,
      };
      setDimensions(prev =>
        prev.width === next.width &&
        prev.height === next.height &&
        prev.chromeHeight === next.chromeHeight
          ? prev
          : next,
      );
      // While the content is height-capped, it can change size without resizing the tooltip.
      const children = new Set(Array.from(content?.children ?? []));
      for (const child of observedChildren) {
        if (!children.has(child)) {
          observer.unobserve(child);
          observedChildren.delete(child);
        }
      }
      for (const child of children) {
        if (!observedChildren.has(child)) {
          observer.observe(child);
          observedChildren.add(child);
        }
      }
    };

    updateDimensions();

    // Children might resize without re-rendering the tooltip.
    // Observe that and trigger re-positioning.
    // Unlike useLayoutEffect, the ResizeObserver does not prevent
    // rendering the old state.
    observer.observe(target);
    return () => observer.disconnect();
  }, [ref, deps]);

  return dimensions;
}
