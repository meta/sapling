/**
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 *
 * @jest-environment jsdom
 */

import {fireEvent, render, screen} from '@testing-library/react';
import {Tooltip} from '../Tooltip';
import {ViewportOverlayRoot} from '../ViewportOverlay';

describe('Tooltip', () => {
  it('keeps the arrow outside the scrollable content', () => {
    render(
      <>
        <Tooltip title="Tooltip text">
          <button>Trigger</button>
        </Tooltip>
        <ViewportOverlayRoot />
      </>,
    );

    fireEvent.mouseEnter(screen.getByRole('button').parentElement as HTMLElement);

    const tooltip = screen.getByRole('tooltip');
    const content = tooltip.querySelector('.tooltip-content');
    const arrow = tooltip.querySelector('.tooltip-arrow');

    expect(tooltip).not.toHaveStyle({overflowY: 'auto'});
    expect(content).toHaveStyle({overflowY: 'auto'});
    expect(content?.parentElement).toBe(tooltip);
    expect(arrow?.parentElement).toBe(tooltip);
  });

  describe('when the content is taller than the viewport', () => {
    const VIEWPORT_HEIGHT = 200;
    const TOOLTIP_HEIGHT = 520;
    const CONTENT_HEIGHT = 500;
    let creatorTop = 0;
    const originals = {
      getBoundingClientRect: HTMLElement.prototype.getBoundingClientRect,
      offsetHeight: Object.getOwnPropertyDescriptor(HTMLElement.prototype, 'offsetHeight'),
      offsetWidth: Object.getOwnPropertyDescriptor(HTMLElement.prototype, 'offsetWidth'),
    };

    // jsdom has no DOMRect constructor.
    const rect = (left: number, top: number, width: number, height: number) =>
      ({
        left,
        top,
        width,
        height,
        right: left + width,
        bottom: top + height,
        x: left,
        y: top,
      }) as DOMRect;

    beforeEach(() => {
      HTMLElement.prototype.getBoundingClientRect = function (this: HTMLElement) {
        if (this === document.body) {
          return rect(0, 0, 800, VIEWPORT_HEIGHT);
        }
        if (this.classList.contains('tooltip-creator')) {
          return rect(100, creatorTop, 20, 20);
        }
        return rect(0, 0, 0, 0);
      };
      Object.defineProperty(HTMLElement.prototype, 'offsetHeight', {
        configurable: true,
        get(this: HTMLElement) {
          if (this.classList.contains('tooltip')) {
            return TOOLTIP_HEIGHT;
          }
          if (this.classList.contains('tooltip-content')) {
            return CONTENT_HEIGHT;
          }
          return 0;
        },
      });
      Object.defineProperty(HTMLElement.prototype, 'offsetWidth', {
        configurable: true,
        get: () => 100,
      });
    });

    afterEach(() => {
      HTMLElement.prototype.getBoundingClientRect = originals.getBoundingClientRect;
      if (originals.offsetHeight) {
        Object.defineProperty(HTMLElement.prototype, 'offsetHeight', originals.offsetHeight);
      }
      if (originals.offsetWidth) {
        Object.defineProperty(HTMLElement.prototype, 'offsetWidth', originals.offsetWidth);
      }
    });

    function openTooltip() {
      render(
        <>
          <Tooltip trigger="click" component={() => <div>Lots of content</div>}>
            <button>Trigger</button>
          </Tooltip>
          <ViewportOverlayRoot />
        </>,
      );
      fireEvent.click(screen.getByRole('button'));
      const tooltip = screen.getByRole('tooltip');
      return {tooltip, content: tooltip.querySelector('.tooltip-content') as HTMLElement};
    }

    it('places the tooltip above, within the viewport, when there is more room above', () => {
      creatorTop = 120;
      const {tooltip, content} = openTooltip();

      // room above: 120 - 5 (gap) - 5 (viewport padding) = 110
      expect(tooltip).toHaveClass('tooltip-top');
      expect(tooltip).toHaveStyle({top: '5px'});
      expect(content).toHaveStyle({maxHeight: `${110 - (TOOLTIP_HEIGHT - CONTENT_HEIGHT)}px`});
    });

    it('places the tooltip below, within the viewport, when there is more room below', () => {
      creatorTop = 80;
      const {tooltip, content} = openTooltip();

      // room below: 200 - (80 + 20 + 5) - 5 = 90
      expect(tooltip).toHaveClass('tooltip-bottom');
      expect(tooltip).toHaveStyle({top: '105px'});
      expect(content).toHaveStyle({maxHeight: `${90 - (TOOLTIP_HEIGHT - CONTENT_HEIGHT)}px`});
    });
  });
});
