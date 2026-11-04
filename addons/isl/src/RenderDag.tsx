/**
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

/* eslint-disable no-bitwise -- intentional bitwise ops for DAG rendering */
import type {JSX, ReactNode} from 'react';
import type {Dag, DagCommitInfo} from './dag/dag';
import type {ExtendedGraphRow} from './dag/render';
import type {HashSet} from './dag/set';

import React from 'react';
import {AnimatedReorderGroup} from './AnimatedReorderGroup';
import {AvatarPattern} from './Avatar';
import {YouAreHereLabel} from './YouAreHereLabel';
import {LinkLine, NodeLine, PadLine} from './dag/render';

import './RenderDag.css';

/* eslint no-bitwise: 0 */

export type RenderDagProps = {
  /** The dag to use */
  dag: Dag;

  /** If set, render a subset. Otherwise, all commits are rendered. */
  subset?: HashSet;

  /** Should "anonymous" parents (rendered as "~" in CLI) be ignored? */
  ignoreAnonymousParents?: boolean;

  /** Disable row reorder animations, such as while pointer dragging. */
  disableReorderAnimation?: boolean;
} & React.HTMLAttributes<HTMLDivElement> &
  RenderFunctionProps;

type RenderFunctionProps = {
  /**
   * How to render a commit.
   *
   * To avoid re-rendering, pass a "static" (ex. not a closure) function,
   * then use hooks (ex. useAtomValue) to trigger re-rendering inside
   * the static function.
   */
  renderCommit?: (info: DagCommitInfo) => JSX.Element | null;

  /**
   * How to render extra stuff below a commit. Default: nothing.
   *
   * To avoid re-rendering, pass a "static" (ex. not a closure) function,
   * then use hooks (ex. useAtomValue) to trigger re-rendering inside
   * the static function.
   */
  renderCommitExtras?: (info: DagCommitInfo, row: ExtendedGraphRow) => null | JSX.Element;

  /**
   * How to render a "glyph" (ex. "o", "x", "@").
   * This should return an SVG element.
   * The SVG viewbox is (-10,-10) to (10,10) (20px * 20px).
   * Default: defaultRenderGlyphSvg, draw a circle.
   *
   * To avoid re-rendering, pass a "static" (ex. not a closure) function,
   * then use hooks (ex. useAtomValue) to trigger re-rendering inside
   * the static function.
   */
  renderGlyph?: (info: DagCommitInfo) => RenderGlyphResult;

  /**
   * Component used to render the row containing the commit's node.
   * This can be used to tweak styles like selection background, border, or add event handlers.
   * It should render a `DivRow` with the given props.
   * This should be a static component (not a closure) to avoid re-rendering.
   * Default: `DivRow`.
   */
  commitRowComponent?: React.ComponentType<CommitRowProps>;
};

export type DivRowProps = {
  left?: JSX.Element | null;
  right?: JSX.Element | null;
} & React.HTMLAttributes<HTMLDivElement> & {['data-commit-hash']?: string};

export type CommitRowProps = DivRowProps & {info: DagCommitInfo};

/**
 * - 'inside-tile': Inside a <Tile />. Must be a svg element. Size decided by <Tile />.
 * - 'replace-tile': Replace the <Tile /> with the rendered result. Size decided by the
 *   rendered result. Can be other elements not just svg. Useful for "You are here".
 */
export type RenderGlyphResult = ['inside-tile', JSX.Element] | ['replace-tile', JSX.Element];

/**
 * Renders a dag. Calculate and render the edges, aka. the left side:
 *
 *   o +--------+
 *   | | commit |
 *   | +--------+
 *   |
 *   | o +--------+
 *   |/  | commit |
 *   o   +--------+
 *   :\
 *   : o +--------+
 *   :   | commit |
 *   :   +--------+
 *   :
 *   o +--------+
 *     | commit |
 *     +--------+
 *
 * The callsite can customize:
 * - What "dag" and what subset of commits to render.
 * - How to render each "commit" (the boxes above).
 * - How to render the glyph (the "o").
 *
 * For a commit with `info.isYouAreHere` set, the "commit" body
 * will be positioned at the right of the "pad" line, not the
 * "node" line, and the default "o" rendering logic will render
 * a blue badge instead.
 *
 * See `DagListProps` for customization options.
 *
 * This component is intended to be used in multiple places,
 * ex. the main dag, "mutation dag", context-menu sub-dag, etc.
 * So it should avoid depending on global atoms.
 */
export function RenderDag(props: RenderDagProps) {
  const {
    dag,
    subset,
    renderCommit,
    renderCommitExtras,
    renderGlyph = defaultRenderGlyph,
    commitRowComponent,
    disableReorderAnimation,
    className,
    ...restProps
  } = props;

  const rows = dag.renderToRows(subset);
  const authors = new Set<string>(
    rows.flatMap(([info]) => (info.phase === 'draft' && info.author.length > 0 ? info.author : [])),
  );

  const renderedRows: Array<JSX.Element> = rows.map(([info, row]) => {
    return (
      <DagRow
        key={info.hash}
        row={row}
        info={info}
        renderCommit={renderCommit}
        renderCommitExtras={renderCommitExtras}
        renderGlyph={renderGlyph}
        commitRowComponent={commitRowComponent}
      />
    );
  });

  const fullClassName = ((className ?? '') + ' render-dag').trimStart();
  return (
    <div className={fullClassName} {...restProps}>
      <SvgPatternList authors={authors} />
      <AnimatedReorderGroup animationDuration={100} disableAnimation={disableReorderAnimation}>
        {renderedRows}
      </AnimatedReorderGroup>
    </div>
  );
}

export function DivRow(props: DivRowProps) {
  const {className, left, right, ...restProps} = props ?? {};
  const fullClassName = `render-dag-row ${className ?? ''}`;
  return (
    <div {...restProps} className={fullClassName}>
      <div className="render-dag-row-left-side">{left}</div>
      <div className="render-dag-row-right-side">{right}</div>
    </div>
  );
}

function DefaultCommitRow({info: _info, ...rest}: CommitRowProps) {
  return <DivRow {...rest} />;
}

function DagRowInner(props: {row: ExtendedGraphRow; info: DagCommitInfo} & RenderFunctionProps) {
  const {
    row,
    info,
    renderGlyph = defaultRenderGlyph,
    renderCommit,
    renderCommitExtras,
    commitRowComponent: CommitRow = DefaultCommitRow,
  } = props;

  // Layout per commit:
  //
  // Each (regular) commit is rendered in 2 rows:
  //
  //    ┌──Row1──────────────────────────────┐
  //    │┌─Left──────────┐┌Right────────────┐│
  //    ││┌PreNode*─────┐││                 ││
  //    │││ | |         │││ (commit body)   ││
  //    ││├Node─────────┤││                 ││
  //    │││ o |         │││                 ││
  //    ││├PostNode*────┤││                 ││
  //    │││ | |         │││                 ││
  //    ││└─────────────┘││                 ││
  //    │└───────────────┘└─────────────────┘│
  //    └────────────────────────────────────┘
  //
  //    ┌──Row2──────────────────────────────┐
  //    │┌─Left──────────┐┌Right────────────┐│
  //    ││┌PostNode*────┐││                 ││
  //    │││ | |         │││                 ││
  //    ││├Term─────────┤││                 ││
  //    │││ | |         │││ (extras)        ││
  //    │││ | ~         │││                 ││
  //    ││├Padding──────┤││                 ││
  //    │││ |           │││                 ││
  //    ││├Link─────────┤││                 ││
  //    │││ |\          │││                 ││
  //    │││ | |         │││                 ││
  //    ││├Ancestry─────┤││                 ││
  //    │││ : |         │││                 ││
  //    │└───────────────┘└─────────────────┘│
  //    └────────────────────────────────────┘
  //
  // Note:
  // - Row1 is used to highlight selection. The "node" line should be
  //   at the center once selected.
  // - The "*" lines (PreNode, PostNode, PostAncestry) have a stretch
  //   height based on the right-side content.
  // - Row2 can be hidden if there is no link line, no ":" ancestry,
  //   and no "extras".
  //
  // Example of "You Are here" special case. "Row1" is split to two
  // rows: "Row0" and "Row1":
  //
  //    ┌──Row0──────────────────────────────┐
  //    │┌─Left─────────────┐                │
  //    ││┌Node────────────┐│                │
  //    │││ | (YouAreHere) ││                │
  //    ││└────────────────┘│                │
  //    │└──────────────────┘                │
  //    └────────────────────────────────────┘
  //    ┌──Row1──────────────────────────────┐
  //    │┌─Left──────────┐┌Right────────────┐│
  //    ││┌PostNode*────┐││                 ││
  //    │││ | |         │││ (commit body)   ││
  //    ││└─────────────┘││                 ││
  //    │└───────────────┘└─────────────────┘│
  //    └────────────────────────────────────┘
  //
  // Note:
  // - Row0's "left" side can have a larger width, to fit the
  //   "irregular" "(YouAreHere)" element.
  // - Row2 is the same in this special case.
  //
  // Also check fbcode/eden/website/src/components/RenderDag.js
  const {linkLine, termLine, nodeLine, ancestryLine, isHead, isRoot, hasIndirectAncestor} = row;

  // By default, the glyph "o" is rendered in a fixed size "Tile".
  // With 'replace-tile' the glyph can define its own rendered element
  // (of dynamic size).
  //
  // 'replace-tile' also moves the "commit" element to the right of
  // pad line, not node line.
  const [glyphPosition, glyph] = renderGlyph(info);
  const isIrregular = glyphPosition === 'replace-tile';
  // isYouAreHere practically matches isIrregular but we treat them as
  // separate concepts. isYouAreHere affects colors, and isIrregular
  // affects layout.
  const color = info.isYouAreHere ? YOU_ARE_HERE_COLOR : undefined;
  const nodeLinePart = (
    <div className="render-dag-row-left-side-line node-line">
      {nodeLine.map((l, i) => {
        if (isIrregular && l === NodeLine.Node) {
          return <React.Fragment key={i}>{glyph}</React.Fragment>;
        }
        // Need stretchY if "glyph" is not "Tile" and has a dynamic height.
        return (
          <NodeTile
            key={i}
            line={l}
            isHead={isHead}
            isRoot={isRoot}
            aboveNodeColor={info.isDot ? YOU_ARE_HERE_COLOR : undefined}
            stretchY={isIrregular && l != NodeLine.Node}
            scaleY={isIrregular ? 0.5 : 1}
            glyph={glyph}
          />
        );
      })}
    </div>
  );

  const preNodeLinePart = (
    <div
      className="render-dag-row-left-side-line pre-node-line grow"
      data-nodecolumn={row.nodeColumn}>
      {row.preNodeLine.map((l, i) => {
        const c = i === row.nodeColumn ? (info.isDot ? YOU_ARE_HERE_COLOR : color) : undefined;
        return <PadTile key={i} line={l} scaleY={0.1} stretchY={true} color={c} />;
      })}
    </div>
  );

  const postNodeLinePart = (
    <div className="render-dag-row-left-side-line post-node-line grow">
      {row.postNodeLine.map((l, i) => {
        const c = i === row.nodeColumn ? color : undefined;
        return <PadTile key={i} line={l} scaleY={0.1} stretchY={true} color={c} />;
      })}
    </div>
  );

  const linkLinePart = linkLine && (
    <div className="render-dag-row-left-side-line link-line">
      {linkLine.map((l, i) => (
        <LinkTile key={i} line={l} color={color} colorLine={row.linkLineFromNode?.[i]} />
      ))}
    </div>
  );

  const stackPaddingPart = linkLine && linkLine.length <= 2 && (
    <div className="render-dag-row-left-side-line stack-padding">
      {linkLine
        // one less than the extra indent added by the link line normally
        .slice(0, -1)
        .map((l, i) => (
          <PadTile key={i} scaleY={0.3} color={color} line={PadLine.Parent} />
        ))}
    </div>
  );

  const termLinePart = termLine && (
    <>
      <div className="render-dag-row-left-side-line term-line-pad">
        {termLine.map((isTerm, i) => {
          const line = isTerm ? PadLine.Ancestor : (ancestryLine.at(i) ?? PadLine.Blank);
          return <PadTile key={i} scaleY={0.25} line={line} />;
        })}
      </div>
      <div className="render-dag-row-left-side-line term-line-term">
        {termLine.map((isTerm, i) => {
          const line = ancestryLine.at(i) ?? PadLine.Blank;
          return isTerm ? <TermTile key={i} /> : <PadTile key={i} line={line} />;
        })}
      </div>
    </>
  );

  const commitPart = renderCommit?.(info);
  const commitExtrasPart = renderCommitExtras?.(info, row);

  const ancestryLinePart = hasIndirectAncestor ? (
    <div className="render-dag-row-left-side-line ancestry-line">
      {ancestryLine.map((l, i) => (
        <PadTile
          key={i}
          scaleY={0.6}
          strokeDashArray="0,2,3,0"
          line={l}
          color={row.parentColumns.includes(i) ? color : undefined}
        />
      ))}
    </div>
  ) : null;

  // Put parts together.

  let row0: JSX.Element | null = null;
  let row1: JSX.Element | null = null;
  let row2: JSX.Element | null = null;
  if (isIrregular) {
    row0 = <CommitRow info={info} left={nodeLinePart} />;
    row1 = <DivRow left={postNodeLinePart} right={commitPart} />;
  } else {
    const left = (
      <>
        {preNodeLinePart}
        {nodeLinePart}
        {postNodeLinePart}
      </>
    );
    row1 = (
      <CommitRow
        info={info}
        className="render-dag-row-commit"
        left={left}
        right={commitPart}
        data-commit-hash={info.hash}
      />
    );
  }

  if (
    linkLinePart != null ||
    termLinePart != null ||
    ancestryLinePart != null ||
    postNodeLinePart != null ||
    commitExtrasPart != null
  ) {
    const left = (
      <>
        {commitExtrasPart && postNodeLinePart}
        {linkLinePart}
        {termLinePart}
        {stackPaddingPart}
        {ancestryLinePart}
      </>
    );
    row2 = <DivRow left={left} right={commitExtrasPart} />;
  }

  return (
    <div
      className="render-dag-row-group"
      data-reorder-id={info.hash}
      data-testid={`dag-row-group-${info.hash}`}>
      {row0}
      {row1}
      {row2}
    </div>
  );
}

const DagRow = React.memo(DagRowInner, (prevProps, nextProps) => {
  return (
    nextProps.info.equals(prevProps.info) &&
    prevProps.row.valueOf() === nextProps.row.valueOf() &&
    prevProps.renderCommit === nextProps.renderCommit &&
    prevProps.renderCommitExtras === nextProps.renderCommitExtras &&
    prevProps.renderGlyph === nextProps.renderGlyph &&
    prevProps.commitRowComponent === nextProps.commitRowComponent
  );
});

export type TileProps = {
  /** Width. Default: defaultTileWidth. */
  width?: number;
  /** Y scale. Default: 1. Decides height. */
  scaleY?: number;
  /**
   * If true, set:
   * - CSS: height: 100% - take up the height of the (flexbox) parent.
   * - CSS: min-height: width * scaleY, i.e. scaleY affects min-height.
   * - SVG: preserveAspectRatio: 'none'.
   * Intended to be only used by PadLine.
   */
  stretchY?: boolean;
  edges?: Edge[];
  /** SVG children. */
  children?: React.ReactNode;
  /** Line width. Default: strokeWidth. */
  strokeWidth?: number;
  /** Dash array. Default: '3,2'. */
  strokeDashArray?: string;
};

/**
 * Represent a line within a box (-1,-1) to (1,1).
 * For example, x1=0, y1=-1, x2=0, y2=1 draws a vertical line in the middle.
 * Default x y values are 0.
 * Flag can be used to draw special lines.
 */
export type Edge = {
  x1?: number;
  y1?: number;
  x2?: number;
  y2?: number;
  flag?: number;
  color?: string;
};

export enum EdgeFlag {
  Dash = 1,
  IntersectGap = 2,
}

const defaultTileWidth = 20;
const defaultStrokeWidth = 2;

/**
 * A tile is a rectangle with edges in it.
 * Children are in SVG.
 */
// eslint-disable-next-line prefer-arrow-callback
function TileInner(props: TileProps) {
  const {
    scaleY = 1,
    width = defaultTileWidth,
    edges = [],
    strokeWidth = defaultStrokeWidth,
    strokeDashArray = '3,2',
    stretchY = false,
  } = props;
  const preserveAspectRatio = stretchY || scaleY < 1 ? 'none' : undefined;
  const height = width * scaleY;
  const style = stretchY ? {height: '100%', minHeight: height} : {};
  // Fill the small caused by scaling, non-integer rounding.
  // When 'x' is at the border (abs >= 10) and 'y' is at the center, use the "gap fix".
  const getGapFix = (x: number, y: number) =>
    y === 0 && Math.abs(x) >= 10 ? 0.5 * Math.sign(x) : 0;
  const paths = edges.map(({x1 = 0, y1 = 0, x2 = 0, y2 = 0, flag = 0, color}, i): JSX.Element => {
    // see getGapFix above.
    const fx1 = getGapFix(x1, y1);
    const fx2 = getGapFix(x2, y2);
    const fy1 = getGapFix(y1, x1);
    const fy2 = getGapFix(y2, x2);

    const sY = scaleY;
    const dashArray = flag & EdgeFlag.Dash ? strokeDashArray : undefined;
    let d;
    if (flag & EdgeFlag.IntersectGap) {
      // This vertical line intercects with a horizontal line visually but it does not mean
      // they connect. Leave a small gap in the middle.
      d = `M ${x1 + fx1} ${y1 * sY + fy1} L 0 -2 M 0 2 L ${x2 + fx2} ${y2 * sY + fy2}`;
    } else if (y1 === y2 || x1 === x2) {
      // Straight line (-----).
      d = `M ${x1 + fx1} ${y1 * sY + fy1} L ${x2 + fx2} ${y2 * sY + fy2}`;
    } else {
      // Curved line (towards center).
      d = `M ${x1 + fx1} ${y1 * sY + fy1} L ${x1} ${y1 * sY} Q 0 0 ${x2} ${y2 * sY} L ${x2 + fx2} ${
        y2 * sY + fy2
      }`;
    }
    return <path d={d} key={i} strokeDasharray={dashArray} stroke={color} />;
  });
  return (
    <svg
      className="render-dag-tile"
      viewBox={`-10 -${scaleY * 10} 20 ${scaleY * 20}`}
      height={height}
      width={width}
      style={style}
      preserveAspectRatio={preserveAspectRatio}>
      <g stroke="var(--foreground)" fill="none" strokeWidth={strokeWidth}>
        {paths}
        {props.children}
      </g>
    </svg>
  );
}
const Tile = React.memo(TileInner);

function NodeTile(
  props: {
    line: NodeLine;
    isHead: boolean;
    isRoot: boolean;
    glyph: JSX.Element;
    /** For NodeLine.Node, the color of the vertical edge above the circle. */
    aboveNodeColor?: string;
  } & TileProps,
) {
  const {line, isHead, isRoot, glyph} = props;
  switch (line) {
    case NodeLine.Ancestor:
      return <Tile {...props} edges={[{y1: -10, y2: 10, flag: EdgeFlag.Dash}]} />;
    case NodeLine.Parent:
      // 10.5 is used instead of 10 to avoid small gaps when the page is zoomed.
      return <Tile {...props} edges={[{y1: -10, y2: 10.5}]} />;
    case NodeLine.Node: {
      const edges: Edge[] = [];
      if (!isHead) {
        edges.push({y1: -10.5, color: props.aboveNodeColor});
      }
      if (!isRoot) {
        edges.push({y2: 10.5});
      }
      return (
        <Tile {...props} edges={edges}>
          {glyph}
        </Tile>
      );
    }
    default:
      return <Tile {...props} edges={[]} />;
  }
}

function PadTile(props: {line: PadLine; color?: string} & TileProps) {
  const {line, color} = props;
  switch (line) {
    case PadLine.Ancestor:
      return <Tile {...props} edges={[{y1: -10, y2: 10, flag: EdgeFlag.Dash, color}]} />;
    case PadLine.Parent:
      return <Tile {...props} edges={[{y1: -10, y2: 10, color}]} />;
    default:
      return <Tile {...props} edges={[]} />;
  }
}

function TermTile(props: TileProps) {
  // "~" in svg.
  return (
    <Tile {...props}>
      <path d="M 0 -10 L 0 -5" strokeDasharray="3,2" />
      <path d="M -7 -5 Q -3 -8, 0 -5 T 7 -5" />
    </Tile>
  );
}

function LinkTile(props: {line: LinkLine; color?: string; colorLine?: LinkLine} & TileProps) {
  const edges = linkLineToEdges(props.line, props.color, props.colorLine);
  return <Tile {...props} edges={edges} />;
}

function linkLineToEdges(linkLine: LinkLine, color?: string, colorLine?: LinkLine): Edge[] {
  const bits = linkLine.valueOf();
  const colorBits = colorLine?.valueOf() ?? 0;
  const edges: Edge[] = [];
  const considerEdge = (parentBits: number, ancestorBits: number, edge: Partial<Edge>) => {
    const present = (bits & (parentBits | ancestorBits)) !== 0;
    const useColor = (colorBits & (parentBits | ancestorBits)) !== 0;
    const dashed = (bits & ancestorBits) !== 0;
    if (present) {
      const flag = edge.flag ?? 0 | (dashed ? EdgeFlag.Dash : 0);
      edges.push({...edge, flag, color: useColor ? color : undefined});
    }
  };
  considerEdge(LinkLine.VERT_PARENT, LinkLine.VERT_ANCESTOR, {
    y1: -10,
    y2: 10,
    flag: bits & (LinkLine.HORIZ_PARENT | LinkLine.HORIZ_ANCESTOR) ? EdgeFlag.IntersectGap : 0,
  });
  considerEdge(LinkLine.HORIZ_PARENT, LinkLine.HORIZ_ANCESTOR, {x1: -10, x2: 10});
  considerEdge(LinkLine.LEFT_MERGE_PARENT, LinkLine.LEFT_MERGE_ANCESTOR, {x1: -10, y2: -10});
  considerEdge(LinkLine.RIGHT_MERGE_PARENT, LinkLine.RIGHT_MERGE_ANCESTOR, {x1: 10, y2: -10});
  considerEdge(LinkLine.LEFT_FORK_PARENT | LinkLine.LEFT_FORK_ANCESTOR, 0, {x1: -10, y2: 10});
  considerEdge(LinkLine.RIGHT_FORK_PARENT | LinkLine.RIGHT_FORK_ANCESTOR, 0, {x1: 10, y2: 10});
  return edges;
}

// Svg patterns for avatar backgrounds. Those patterns are referred later by `RegularGlyph`.
function SvgPatternList(props: {authors: Iterable<string>}) {
  return (
    <svg className="render-dag-svg-patterns" viewBox={`-10 -10 20 20`}>
      <defs>
        {[...props.authors].map(author => (
          <SvgPattern author={author} key={author} />
        ))}
      </defs>
    </svg>
  );
}

function authorToSvgPatternId(author: string) {
  return 'avatar-pattern-' + author.replace(/[^A-Z0-9a-z]/g, '_');
}

function SvgPatternInner(props: {author: string}) {
  const {author} = props;
  const id = authorToSvgPatternId(author);
  return (
    <AvatarPattern
      size={DEFAULT_GLYPH_RADIUS * 2}
      username={author}
      id={id}
      fallbackFill="var(--foreground)"
    />
  );
}

const SvgPattern = React.memo(SvgPatternInner);

const YOU_ARE_HERE_COLOR = 'var(--button-primary-hover-background)';
const DEFAULT_GLYPH_RADIUS = (defaultTileWidth * 7) / 20;

function RegularGlyphInner({info}: {info: DagCommitInfo}) {
  const stroke = info.isDot ? YOU_ARE_HERE_COLOR : 'var(--foreground)';
  const r = DEFAULT_GLYPH_RADIUS;
  const strokeWidth = defaultStrokeWidth * 0.9;
  const isObsoleted = info.successorInfo != null;
  let fill = 'var(--foreground)';
  let extraSvgElement = null;
  if (info.phase === 'draft') {
    if (isObsoleted) {
      // "/" inside the circle (similar to "x" in CLI) to indicate "obsoleted".
      fill = 'var(--background)';
      const pos = r / Math.sqrt(2) - strokeWidth;
      extraSvgElement = (
        <path
          d={`M ${-pos} ${pos} L ${pos} ${-pos}`}
          stroke={stroke}
          strokeWidth={strokeWidth}
          strokeLinecap="round"
        />
      );
    } else if (info.author.length > 0) {
      // Avatar for draft, non-obsoleted commits.
      const id = authorToSvgPatternId(info.author);
      fill = `url(#${id})`;
    }
  }

  return (
    <>
      <circle cx={0} cy={0} r={r} fill={fill} stroke={stroke} strokeWidth={strokeWidth} />
      {extraSvgElement}
    </>
  );
}

export const RegularGlyph = React.memo(RegularGlyphInner, (prevProps, nextProps) => {
  const prevInfo = prevProps.info;
  const nextInfo = nextProps.info;
  return nextInfo.equals(prevInfo);
});

/**
 * The default "You are here" glyph - render as a blue bubble. Intended to be used in
 * different `RenderDag` configurations.
 *
 * If you want to customize the rendering for the main graph, or introducing dependencies
 * that seem "extra" (like code review states, operation-related progress state), consider
 * passing the `renderGlyph` prop to `RenderDag` instead. See `CommitTreeList` for example.
 */
export function YouAreHereGlyph({
  info,
  title,
  badgeChildren,
  children,
}: {
  info: DagCommitInfo;
  /** Override the default label text (`info.description`, i.e. "You are here"). */
  title?: string;
  /** Rendered inside the blue badge, after the label text. */
  badgeChildren?: ReactNode;
  children?: ReactNode;
}) {
  return (
    <YouAreHereLabel
      title={title ?? info.description}
      badgeChildren={badgeChildren}
      style={{marginLeft: -defaultStrokeWidth * 1.5}}>
      {children}
    </YouAreHereLabel>
  );
}

export function defaultRenderGlyph(info: DagCommitInfo): RenderGlyphResult {
  if (info.isYouAreHere) {
    return ['replace-tile', <YouAreHereGlyph key="glyph" info={info} />];
  } else {
    return ['inside-tile', <RegularGlyph key="glyph" info={info} />];
  }
}
